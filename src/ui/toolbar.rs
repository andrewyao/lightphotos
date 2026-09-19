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

            let t = t();
            ui.label(t.rating_filter);
            let resp = ui.selectable_label(app.filter().is_none(), t.all);
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
                (Cmp::Gte, "\u{2265}", t.at_least_n_stars),
                (Cmp::Eq, "=", t.exactly_n_stars),
                (Cmp::Lte, "\u{2264}", t.at_most_n_stars),
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
                let resp = ui.add(star).on_hover_text((t.show_rated)(cmp_sym, n));
                if resp.clicked() {
                    out.actions.push(UiAction::SetFilter(Some((sel_cmp, n))));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;
            }
            ui.separator();
            let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
            let resp = ui
                .selectable_label(unrated, t.unrated)
                .on_hover_text(t.unrated_tip);
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
                        egui::Button::selectable(app.bursts_on(), t.bursts),
                    )
                    .on_hover_text(if filter_active {
                        t.bursts_needs_no_filter
                    } else {
                        t.bursts_tip
                    });
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleBursts);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;

                let resp = ui
                    .selectable_label(app.dupes_on(), t.duplicates)
                    .on_hover_text(t.duplicates_tip);
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
                        egui::Button::selectable(app.eyes_filter_on(), t.eyes_closed),
                    )
                    .on_hover_text(if grouped || app.eyes_filter_on() {
                        t.eyes_closed_tip
                    } else {
                        t.eyes_closed_needs_grouping
                    });
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleEyesClosed);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            }

            // Survey's own header already counts its photos.
            if app.mode() == ViewMode::Grid {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak((t.n_photos)(app.visible_len()));
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
    let t = t();
    egui::Panel::top("selection_bar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Buttons are taller than a label; keep the row one height.
            ui.set_min_height(ui.spacing().interact_size.y);
            if n == 0 {
                ui.weak(t.no_selection);
                return;
            }
            ui.strong((t.n_selected)(n));
            ui.separator();
            egui::ComboBox::from_id_salt("bulk_star")
                .selected_text(t.rate_menu)
                .show_ui(ui, |ui| {
                    for s in (1u8..=5).rev() {
                        if ui.button(star_string(s)).clicked() {
                            out.actions.push(UiAction::RequestBulk(BulkKind::Rate(s)));
                        }
                    }
                    if ui.button(t.clear_rating).clicked() {
                        out.actions.push(UiAction::RequestBulk(BulkKind::Rate(0)));
                    }
                });
            if ui
                .button(t.auto_tone)
                .on_hover_text(t.auto_tone_selection_tip)
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
            }
            ui.separator();
            if ui
                .add_enabled(n == 1, egui::Button::new(t.copy_settings))
                .on_hover_text(t.copy_settings_tip)
                .on_disabled_hover_text(t.copy_settings_needs_one)
                .clicked()
            {
                out.actions.push(UiAction::CopySettings);
            }
            let apply = ui
                .add_enabled(
                    app.has_copied_settings(),
                    egui::Button::new(t.apply_settings),
                )
                .on_disabled_hover_text(t.apply_settings_needs_copy);
            if apply.clicked() {
                out.actions
                    .push(UiAction::RequestBulk(BulkKind::ApplySettings));
            }
            if let Some(name) = app.copied_settings_name() {
                ui.weak((t.settings_from)(&name));
            }
            ui.separator();
            if ui
                .button(t.export_jpg)
                .on_hover_text(t.export_jpg_tip)
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::Export));
            }

            // Destructive, so it sits apart from the others at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new(t.delete).color(theme::DANGER_RED))
                    .on_hover_text(t.delete_selection_tip)
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
                }
            });
        });
    });
}
