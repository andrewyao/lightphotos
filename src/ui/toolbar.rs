use super::*;

use crate::app::{App, Region, SHOW_GROUPING_TOOLS};
use crate::navigation::Cmp;

/// The Grid and Survey toolbar: rating filter, grouping toggles (while
/// `SHOW_GROUPING_TOOLS` is on), and bulk actions on the selection.
pub(super) fn grid_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("grid_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Keyboard-cursor index of each control, matching
            // `App::activate_toolbar_focus`. The bulk actions are not in the
            // keyboard cycle because their count changes with the selection.
            let mut idx = 0usize;

            ui.label("Filter:");
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
                idx += 1;
            }

            if let Some(name) = app.copied_settings_name() {
                ui.separator();
                ui.label(format!("Settings from: {name}"));
            }

            // Every bulk action opens a confirm modal before it runs.
            let n = app.selection_count();
            if n > 0 {
                ui.separator();
                ui.label(format!("{n} selected"));
                egui::ComboBox::from_id_salt("bulk_star")
                    .selected_text("Apply \u{2605}")
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
                    .add_enabled(n == 1, egui::Button::new("Copy Settings"))
                    .on_hover_text("Copy this photo's develop settings (Cmd+Shift+C)")
                    .on_disabled_hover_text("Select a single photo to copy its settings")
                    .clicked()
                {
                    out.actions.push(UiAction::CopySettings);
                }
                if ui
                    .add_enabled(
                        app.has_copied_settings(),
                        egui::Button::new("Apply Settings"),
                    )
                    .on_disabled_hover_text("Copy settings from a photo first")
                    .clicked()
                {
                    out.actions
                        .push(UiAction::RequestBulk(BulkKind::ApplySettings));
                }
                if ui
                    .button("Auto Tone")
                    .on_hover_text(
                        "Set each selected photo's tone sliders from its own histogram (Cmd+Shift+U)",
                    )
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
                }
                if ui
                    .button("Export JPG")
                    .on_hover_text("Export each selected photo as a baked JPG")
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Export));
                }
                if ui
                    .button("Delete")
                    .on_hover_text(if cfg!(target_arch = "wasm32") {
                        "Permanently delete selected photos; cannot be undone (Delete)"
                    } else {
                        "Move selected photos to the Trash (Delete)"
                    })
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
                }
            }

            // `?` is the last keyboard-focusable control.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui.button("?").on_hover_text("Keyboard shortcuts (?)");
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleHelp);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            });

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}

/// The Loupe toolbar has no filter, grouping, or bulk controls. Changing the
/// filter while a photo is open could drop that photo out of the Grid's
/// selection and break rating it. The only focusable control is `?` at index
/// 0, matching `App::LOUPE_TOOLBAR_CONTROLS`.
pub(super) fn loupe_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("loupe_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            let idx = 0usize;

            // Right-to-left, so `?` lands in the top-right corner.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui.button("?").on_hover_text("Keyboard shortcuts (?)");
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleHelp);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);

            });

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}
