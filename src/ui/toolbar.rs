use super::*;
use super::modals::rating_histogram;

use crate::app::{App, Region, ViewMode};
use crate::navigation::Cmp;


/// The always-visible global toolbar, drawn once above both modes. Currently
/// hosts the rating filter (`All` + 5 stars → show photos rated ≥ N).
pub(super) fn global_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("global_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Index of the keyboard-focusable control being drawn, bumped after
            // each one — the Phase-1 stable set only (see `activate_toolbar_focus`);
            // the histogram bars and selection-dependent bulk actions below aren't
            // in the keyboard cycle yet since their count varies frame to frame.
            let mut idx = 0usize;

            ui.label("Filter:");
            // `All` clears the filter; selected only when no filter is set.
            let resp = ui.selectable_label(app.filter().is_none(), "All");
            if resp.clicked() {
                out.actions.push(UiAction::SetFilter(None));
            }
            toolbar_focus_sync(ui, app, idx, &resp, out);
            idx += 1;
            ui.separator();
            // Comparator selector: the mode (≥ / = / ≤) applied to the star
            // clicked next. Sticky, so it's highlighted even with no filter set.
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
            // The active star-level filter (a 1..=5 threshold), if any. Drives
            // which stars light up; `Eq` lights only star N, `Gte`/`Lte` cascade.
            let (active_cmp, active_n) = match app.filter() {
                Some((c, v)) if (1..=5).contains(&v) => (Some(c), v),
                _ => (None, 0),
            };
            let cmp_sym = match sel_cmp {
                Cmp::Gte => "\u{2265}",
                Cmp::Eq => "=",
                Cmp::Lte => "\u{2264}",
            };
            // Five clickable stars: click star N → filter (current comparator, N).
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
            // Show only unstarred photos (rating == 0), mutually exclusive with ≥N.
            let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
            let resp = ui
                .selectable_label(unrated, "Unrated")
                .on_hover_text("Show only photos with no rating");
            if resp.clicked() {
                // Toggle off back to All when it's already active.
                let next = if unrated { None } else { Some((Cmp::Eq, 0)) };
                out.actions.push(UiAction::SetFilter(next));
            }
            toolbar_focus_sync(ui, app, idx, &resp, out);
            idx += 1;

            // Best-of-burst toggle. Disabled while a filter is active (bursts
            // need the whole, unfiltered folder to be meaningful).
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

            // Rating-distribution histogram (folder-wide), clickable to filter.
            ui.separator();
            rating_histogram(ui, app, out);

            // `?` opens the keyboard-shortcut help.
            ui.separator();
            let resp = ui.button("?").on_hover_text("Keyboard shortcuts (?)");
            if resp.clicked() {
                out.actions.push(UiAction::ToggleHelp);
            }
            toolbar_focus_sync(ui, app, idx, &resp, out);
            idx += 1;

            // Show which photo's develop settings are on the clipboard, if any.
            if let Some(name) = app.copied_settings_name() {
                ui.separator();
                ui.label(format!("Settings from: {name}"));
            }

            // Bulk actions on the current selection, shown only when something is
            // selected. Every bulk op is confirmed via a modal before it runs.
            let n = app.selection_count();
            if n > 0 {
                ui.separator();
                ui.label(format!("{n} selected"));
                // Apply Star: a 1–5 dropdown plus a clear-rating entry.
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
                // Copy settings comes from a single photo, so it's only enabled
                // when exactly one is selected; paste onto the whole selection.
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
                // Export every selected photo as a baked JPG.
                if ui
                    .button("Export JPG")
                    .on_hover_text("Export each selected photo as a baked JPG")
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Export));
                }
                // Move the selection to the Trash (confirmed).
                if ui
                    .button("Delete")
                    .on_hover_text("Move selected photos to the Trash (Delete)")
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
                }
            }

            // Loupe / Grid mode toggle, pinned to the far right. In a
            // right-to-left layout the first widget is the rightmost, so add
            // `G` first to read "E  G" left-to-right. Added in this order, `G`
            // naturally lands at `idx` (-> EnterGrid) and `E` at `idx + 1` (->
            // EnterLoupe), matching `activate_toolbar_focus`.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui
                    .selectable_label(app.mode() == ViewMode::Grid, "G")
                    .on_hover_text("Grid (G)");
                if resp.clicked() {
                    out.actions.push(UiAction::EnterGrid);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);

                let resp = ui
                    .selectable_label(app.mode() == ViewMode::Loupe, "E")
                    .on_hover_text("Loupe / edit (E)");
                if resp.clicked() {
                    out.actions.push(UiAction::EnterLoupe);
                }
                toolbar_focus_sync(ui, app, idx + 1, &resp, out);
            });

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}
