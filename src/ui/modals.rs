use super::*;

use crate::app::App;

/// Confirms a pending bulk action. Cancel, Esc, or a backdrop click dismisses it.
pub(super) fn confirm_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(prompt) = app.pending_bulk_prompt() else {
        return;
    };
    let resp = egui::Modal::new(egui::Id::new("bulk_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading("Confirm");
        ui.add_space(6.0);
        ui.label(prompt);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                out.actions.push(UiAction::CancelBulk);
            }
            if ui.button("Confirm").clicked() {
                out.actions.push(UiAction::ConfirmBulk);
            }
        });
    });
    if resp.should_close() {
        out.actions.push(UiAction::CancelBulk);
    }
}

/// Confirms quit, opened by Esc in the grid.
pub(super) fn quit_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.pending_quit() {
        return;
    }
    let resp = egui::Modal::new(egui::Id::new("quit_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(300.0);
        ui.heading("Quit LightPhotos?");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                out.actions.push(UiAction::CancelQuit);
            }
            if ui.button("Quit").clicked() {
                out.actions.push(UiAction::ConfirmQuit);
            }
        });
    });
    // A backdrop click cancels. A second Esc quits, matching `App::handle_key`.
    if resp.should_close() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            out.actions.push(UiAction::ConfirmQuit);
        } else {
            out.actions.push(UiAction::CancelQuit);
        }
    }
}

/// The keyboard-shortcut list, toggled by `?`.
pub(super) fn help_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_help() {
        return;
    }
    // The primary modifier: Cmd in the native macOS app, Ctrl elsewhere.
    macro_rules! primary {
        ($keys:literal) => {
            if cfg!(all(not(target_arch = "wasm32"), target_os = "macos")) {
                concat!("Cmd", $keys)
            } else {
                concat!("Ctrl", $keys)
            }
        };
        ($a:literal or $b:literal) => {
            if cfg!(all(not(target_arch = "wasm32"), target_os = "macos")) {
                concat!("Cmd", $a, " or Cmd", $b)
            } else {
                concat!("Ctrl", $a, " or Ctrl", $b)
            }
        };
    }
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Navigate",
            &[
                ("Enter or Space", "Open selected photo (in library)"),
                ("Esc", "Back to library (in editor)"),
                ("\u{2190} / \u{2192}", "Previous / next photo (in editor)"),
                (
                    "\u{2190} \u{2192} \u{2191} \u{2193}",
                    "Move selection in library grid",
                ),
                ("E / G", "Editor / library"),
            ],
        ),
        (
            "Select",
            &[
                (primary!("+A"), "Select all in current folder"),
                ("Shift+Click", "Range-select"),
                (primary!("+Click"), "Toggle individual selection"),
                ("Shift+arrows", "Extend selection (library)"),
            ],
        ),
        (
            "Rate and label",
            &[
                ("0 1 2 3 4 5", "Set star rating 0\u{2013}5"),
                (
                    "Shift+1 \u{2013} 5",
                    "Set color label (red / yellow / green / blue / purple)",
                ),
                ("Shift+0", "Clear color label"),
            ],
        ),
        (
            "Zoom and pan (in editor)",
            &[
                (
                    "Space",
                    "Cycle zoom: fit \u{2192} 2\u{d7} fit \u{2192} 100%",
                ),
                (primary!("+0" or "+)"), "Fit to window"),
                (primary!("+1" or "+!"), "100% (1:1 pixel)"),
                (primary!("++" or "+="), "Zoom in (20% step)"),
                (primary!("+\u{2212}"), "Zoom out (20% step)"),
                ("\u{2191} / \u{2193}", "Zoom in / out (10% step)"),
                ("Shift+Scroll", "Pan horizontally"),
                ("Alt+Scroll", "Pan vertically"),
                ("Shift+Alt+Scroll", "Trackpad zoom"),
                ("Space+Drag", "Pan"),
            ],
        ),
        (
            "Edit",
            &[
                ("[", "Rotate image \u{2212}90\u{b0}"),
                ("]", "Rotate image +90\u{b0}"),
                ("C", "Crop"),
                ("Y", "Before / after compare"),
                (primary!("+U"), "Auto Tone this photo"),
                (primary!("+Shift+U"), "Auto Tone the selection"),
                (primary!("+Shift+C"), "Copy develop settings"),
                (primary!("+Shift+Y"), "Apply settings to selection"),
                ("X", "Export selected as JPG"),
                (
                    "Delete",
                    if cfg!(target_arch = "wasm32") {
                        "Permanently delete; cannot be undone"
                    } else {
                        "Move to Trash"
                    },
                ),
            ],
        ),
        (
            "Culling",
            &[
                ("B", "Best-of-burst badges (library)"),
                ("D", "Duplicate-group badges (library)"),
                (
                    "Click badge",
                    "Survey the group: \u{2190}/\u{2192} pick, Enter keeps best, Esc closes",
                ),
            ],
        ),
        (
            "Keyboard focus",
            &[
                ("F6 / Shift+F6", "Cycle focus between regions"),
                (
                    "Tab / Shift+Tab",
                    "Next / previous item in the focused region",
                ),
                ("?", "Show or hide this help"),
            ],
        ),
    ];
    let resp = egui::Modal::new(egui::Id::new("help_overlay")).show(ui.ctx(), |ui| {
        ui.set_width(460.0);
        ui.heading("Keyboard shortcuts");
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .max_height(ui.ctx().content_rect().height() * 0.7)
            .show(ui, |ui| {
                for (i, (title, rows)) in SECTIONS.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(10.0);
                    }
                    ui.label(egui::RichText::new(*title).small().weak());
                    ui.separator();
                    egui::Grid::new(("help_grid", i))
                        .num_columns(2)
                        .min_col_width(150.0)
                        .spacing([18.0, 6.0])
                        .striped(true)
                        .show(ui, |ui| {
                            for (key, desc) in *rows {
                                ui.label(egui::RichText::new(*key).strong());
                                ui.label(*desc);
                                ui.end_row();
                            }
                        });
                }
            });
        ui.add_space(10.0);
        if ui.button("Close").clicked() {
            out.actions.push(UiAction::ToggleHelp);
        }
    });
    if resp.should_close() {
        out.actions.push(UiAction::ToggleHelp);
    }
}
