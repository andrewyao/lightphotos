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
    }
    const SHORTCUTS: &[(&str, &str)] = &[
        ("Enter or Space", "Open the selected photo (library)"),
        ("Esc", "Back to the library (editor)"),
        ("\u{2190} / \u{2192}", "Previous / next photo (editor)"),
        ("\u{2190} \u{2192} \u{2191} \u{2193}", "Move the selection (library)"),
        ("Shift + arrows", "Extend the selection (library)"),
        (primary!(" + A"), "Select all in the current folder"),
        (
            primary!("-click / Shift-click"),
            "Toggle one photo / select a range",
        ),
        ("0 \u{2013} 5", "Set star rating (0 clears)"),
        (
            "Shift + 1 \u{2013} 5",
            "Color label: red / yellow / green / blue / purple",
        ),
        ("Shift + 0", "Clear the color label"),
        ("Space", "Cycle zoom: fit \u{2192} 2\u{d7} fit \u{2192} 100% (editor)"),
        (primary!(" + 0"), "Fit to window"),
        (primary!(" + 1"), "100% (1:1 pixels)"),
        (primary!(" + = / \u{2212}"), "Zoom in / out 20%"),
        ("\u{2191} / \u{2193}", "Zoom in / out 10% (editor)"),
        ("Shift + scroll", "Pan horizontally"),
        ("Alt + scroll", "Pan vertically"),
        ("Shift + Alt + scroll", "Zoom"),
        ("Space + drag", "Pan"),
        ("[ / ]", "Rotate \u{2212}90\u{b0} / +90\u{b0}"),
        ("F6 / Shift+F6", "Cycle keyboard focus between regions"),
        (
            "Tab / Shift+Tab",
            "From the folder tree, jump to the grid's first image; in the grid, step to the next/previous image",
        ),
        ("E / G", "Loupe / Grid"),
        ("C", "Crop"),
        ("B", "Best-of-burst badges (grid)"),
        ("D", "Duplicate-group badges (grid)"),
        (
            "Click badge",
            "Open Survey Mode on that duplicate group; Left/Right focus a member, Enter keeps best/rejects rest, Esc closes",
        ),
        ("Y", "Before / after compare"),
        (primary!(" + U"), "Auto Tone this photo"),
        (primary!(" + Shift + U"), "Auto Tone the selection"),
        (primary!(" + Shift + C"), "Copy develop settings"),
        (primary!(" + Shift + Y"), "Apply settings to selection"),
        ("X", "Export selected as JPG"),
        (
            "Delete",
            if cfg!(target_arch = "wasm32") {
                "Permanently delete; cannot be undone"
            } else {
                "Move to Trash"
            },
        ),
        ("?", "This help"),
    ];
    let resp = egui::Modal::new(egui::Id::new("help_overlay")).show(ui.ctx(), |ui| {
        ui.set_width(420.0);
        ui.heading("Keyboard shortcuts");
        ui.add_space(6.0);
        egui::Grid::new("help_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for (key, desc) in SHORTCUTS {
                    ui.label(egui::RichText::new(*key).strong());
                    ui.label(*desc);
                    ui.end_row();
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
