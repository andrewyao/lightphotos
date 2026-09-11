use super::*;

use crate::app::{App, Region, SHOW_GROUPING_TOOLS};
use crate::navigation::Cmp;

/// The Grid/Survey toolbar, drawn once from `ui::draw` above the middle
/// column. Hosts the rating filter (`All` + 5 stars → show photos rated ≥
/// N), the selection-dependent bulk actions, and — while
/// `SHOW_GROUPING_TOOLS` is on, which it currently is not — the
/// Bursts/Duplicates/Eyes-closed grouping toggles. All Grid concepts, which is why Loupe gets a separate, much
/// smaller toolbar (`loupe_toolbar` below) instead of this one merely
/// disabled: filtering/grouping/bulk-selecting don't apply to "one photo,
/// open for editing", and letting the filter stay live while a photo was
/// open was the root cause of a real bug (see git history) — a Loupe photo
/// could get silently knocked out of the Grid's filtered selection cursor,
/// breaking rating for it.
pub(super) fn grid_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("grid_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Index of the keyboard-focusable control being drawn, bumped after
            // each one — the Phase-1 stable set only (see `activate_toolbar_focus`);
            // the selection-dependent bulk actions below aren't in the keyboard
            // cycle yet since their count varies frame to frame.
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

            // Bursts / Duplicates / Eyes-closed, hidden for now behind
            // `SHOW_GROUPING_TOOLS` (the `B` and `D` keys still toggle the
            // first two). Their focus indices are still theirs while they are
            // hidden — `App::TOOLBAR_CONTROLS` counts off the same flag, so
            // the `?` button that follows keeps whatever index is left over.
            if SHOW_GROUPING_TOOLS {
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

                // Content-duplicate (dHash) grouping toggle. Independent of the
                // filter — unlike Bursts, this grouping is order-independent.
                let resp = ui
                    .selectable_label(app.dupes_on(), "Duplicates")
                    .on_hover_text("Group visually-similar frames and badge them (D)");
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleDupes);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;

                // "Eyes closed" filter. Enabled only alongside one of the grouping
                // toggles, because the face pass those drive is the only thing that
                // fills the cache this reads — Vision decodes at full resolution to
                // find faces, too heavy to run folder-wide unasked. Turn one of them
                // on and this narrows to whatever blinks the pass has found.
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
                // Delete the selection (confirmed).
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

            // Help, pinned to the far right so `?` sits in the top-right
            // corner, always visible. The clickable E/G mode toggle that used
            // to sit to its left was dropped — the `E`/`G` keys still switch
            // modes. It's the last
            // keyboard-focusable control, at `idx` (ToggleHelp), matching
            // `activate_toolbar_focus`'s Grid arm.
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

/// The Loupe toolbar: deliberately minimal, unlike `grid_toolbar` above.
/// Every Grid-only control (filter, grouping, bulk actions) is dropped
/// rather than shown-disabled — the photo currently open has its own
/// keyboard shortcuts (rate with 1-5, copy settings Cmd+Shift+C, delete via
/// Delete) and the Develop panel/info bar for everything else, so there's
/// nothing Grid-toolbar-shaped left to offer here. The only focusable
/// control is `?` at index 0 — must match `App::activate_toolbar_focus`'s
/// Loupe-mode arm and `toolbar_control_count`'s `LOUPE_TOOLBAR_CONTROLS`.
pub(super) fn loupe_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("loupe_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            let idx = 0usize;

            // Help + a TEMPORARY DEBUG decode-tier readout, pinned to the far
            // right. Right-to-left: `?` is added first so it sits in the
            // top-right corner (always visible); the tier letter sits to its
            // left. The old Loupe/Grid toggle was dropped here — `E`/`G` keys
            // still switch modes.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = ui.button("?").on_hover_text("Keyboard shortcuts (?)");
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleHelp);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);

                // Which decode tier the Loupe currently has on the GPU:
                // T=thumb, S=speed, P=preview/quality, F=full. Debug only —
                // pairs with the `[TIER]` window-title prefix (see
                // `App::debug_tier_label`). Non-interactive, so it's not in
                // the F6 focus cycle.
                let tier = match app.debug_tier_label {
                    "THUMB" => "T",
                    "SPEED" => "S",
                    "QUALITY" => "P",
                    "FULL" => "F",
                    _ => "\u{2013}",
                };
                ui.label(tier).on_hover_text(
                    "Loupe decode tier (debug): T=thumb  S=speed  P=preview  F=full",
                );
            });

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}
