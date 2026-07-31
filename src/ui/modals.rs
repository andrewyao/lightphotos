use super::*;

use crate::app::App;
use crate::navigation::Cmp;


/// A modal confirming a pending bulk action. Confirm runs it; Cancel / Esc /
/// clicking the backdrop dismisses it.
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
    // Backdrop click / Escape → treat as cancel.
    if resp.should_close() {
        out.actions.push(UiAction::CancelBulk);
    }
}

/// A modal confirming quit (Esc in the grid). Quit exits the app; Cancel / Esc /
/// clicking the backdrop keeps it running.
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
    // Backdrop click cancels; Escape confirms, matching App::handle_key.
    if resp.should_close() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            out.actions.push(UiAction::ConfirmQuit);
        } else {
            out.actions.push(UiAction::CancelQuit);
        }
    }
}

/// A compact per-rating histogram (0..=5 stars) of the current folder. Each bar
/// is clickable to filter to exactly that rating (click the active bar to clear).
pub(super) fn rating_histogram(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let counts = app.rating_counts();
    if counts.iter().all(|&c| c == 0) {
        return; // no folder loaded
    }
    let max = counts.iter().copied().max().unwrap_or(1).max(1) as f32;
    let bar_w = 12.0;
    let gap = 3.0;
    let h = 24.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(6.0 * (bar_w + gap), h), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    for k in 0u8..6 {
        let x = rect.min.x + k as f32 * (bar_w + gap);
        let col = egui::Rect::from_min_size(egui::pos2(x, rect.min.y), egui::vec2(bar_w, h));
        let frac = counts[k as usize] as f32 / max;
        let bh = if counts[k as usize] == 0 {
            0.0
        } else {
            (frac * (h - 1.0)).max(2.0)
        };
        let bar = egui::Rect::from_min_max(egui::pos2(x, rect.max.y - bh), col.max);
        let active = matches!(app.filter(), Some((Cmp::Eq, v)) if v == k);
        let color = if active {
            theme::SELECTION_BLUE
        } else if k == 0 {
            egui::Color32::from_gray(120)
        } else {
            theme::STAR_GOLD
        };
        painter.rect_filled(bar, 1.0, color);
        let resp = ui
            .interact(col, ui.id().with(("rating_hist", k)), egui::Sense::click())
            .on_hover_text(format!(
                "{} photo(s) rated {}\u{2605}",
                counts[k as usize], k
            ));
        if resp.clicked() {
            // Toggle: clicking the active bar clears the filter.
            let next = if active { None } else { Some((Cmp::Eq, k)) };
            out.actions.push(UiAction::SetFilter(next));
        }
    }
}

/// The keyboard-shortcut help overlay (toggled by `?`). A modal listing the
/// key bindings; Close / Esc / backdrop dismisses it.
pub(super) fn help_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.show_help() {
        return;
    }
    const SHORTCUTS: &[(&str, &str)] = &[
        ("F6 / Shift+F6", "Cycle keyboard focus between regions"),
        (
            "Tab / Shift+Tab",
            "From the folder tree, jump to the grid's first image; in the grid, step to the next/previous image",
        ),
        ("Arrow keys", "Navigate/adjust the focused region's content"),
        ("Shift + arrows", "Extend selection (grid)"),
        ("Cmd + A", "Select all"),
        ("Click / Cmd-click / Shift-click", "Select / toggle / range"),
        ("Enter", "Enter the focused region / open photo / expand folder"),
        ("E / G", "Loupe / Grid"),
        (
            "Esc",
            "Back out one focus level, then quit-confirm; in Loupe, back to Grid; from the grid, back to the folder tree",
        ),
        ("1 \u{2013} 5 / 0", "Rate / clear rating"),
        ("Shift + 1 \u{2013} 5", "Filter \u{2265} N stars"),
        ("C", "Crop"),
        ("B", "Best-of-burst badges (grid)"),
        ("D", "Duplicate-group badges (grid)"),
        (
            "Click badge",
            "Open Survey Mode on that duplicate group; Left/Right focus a member, Enter keeps best/rejects rest, Esc closes",
        ),
        ("Cmd + [ or ]", "Rotate 90\u{b0} clockwise or anti-clockwise"),
        ("Y", "Before / after compare"),
        ("Cmd + Shift + C", "Copy develop settings"),
        ("Cmd + Shift + Y", "Apply settings to selection"),
        ("X", "Export selected as JPG"),
        ("Delete", "Move to Trash"),
        ("+ / \u{2212}", "Thumbnail size (grid)"),
        ("Alt + 0", "Reset zoom (100%)"),
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
