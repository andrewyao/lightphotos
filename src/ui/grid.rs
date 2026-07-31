use super::*;
use std::path::Path;

use crate::app::{App, Region};
use crate::burst::BurstMark;
use crate::duplicates::DuplicateMark;


/// Left folder-tree sidebar, rooted at the opened folder. Shown in both the grid
/// and the loupe so the folder structure is always visible.
pub(super) fn draw_folders_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.folders_visible() {
        return;
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let content_width = app
        .folder_root()
        .map(|root| folder_content_width(ui, app, root.as_path(), 0, &font))
        .unwrap_or(0.0);
    let panel_width = (content_width + 24.0)
        .max(220.0)
        .min((ui.available_width() - 96.0).max(220.0));
    egui::Panel::left("folders")
        .resizable(false)
        .exact_size(panel_width)
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(root) = app.folder_root() {
                        folder_node(ui, app, &root, 0, out);
                    }
                });
        });
}

/// Width needed by the currently visible folder rows, measured with the same
/// body font used by `selectable_label`. The side panel follows this width
/// automatically; it has no user resize affordance.
pub(super) fn folder_content_width(
    ui: &egui::Ui,
    app: &App,
    path: &Path,
    depth: usize,
    font: &egui::FontId,
) -> f32 {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let label = format!("{}  {name}", if app.is_expanded(path) { "▼" } else { "▶" });
    let text_width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label, font.clone(), egui::Color32::WHITE)
            .size()
            .x
    });
    let own_width = depth as f32 * 14.0 + text_width;
    if app.is_expanded(path) {
        app.subdirs(path)
            .iter()
            .map(|child| folder_content_width(ui, app, child, depth + 1, font))
            .fold(own_width, f32::max)
    } else {
        own_width
    }
}

pub(super) fn draw_grid(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let thumb_px = app.thumb_px();
    let sel = app.sel();

    // Left folder-tree sidebar, rooted at the opened folder.
    draw_folders_panel(ui, app, out);

    egui::Panel::top("toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Size");
            let mut px = thumb_px as f32;
            if ui
                .add(egui::Slider::new(&mut px, 96.0..=512.0).show_value(false))
                .changed()
            {
                out.actions.push(UiAction::SetThumbPx(px.round() as u32));
            }
            ui.separator();
            ui.label(format!("{} photos", app.visible_len()));
        });
    });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let cell = thumb_px as f32;
        let spacing = ui.spacing().item_spacing.x;
        // Leave room for the scrollbar so the last column isn't clipped (cols is
        // fixed before we enter the scroll area, where the inner width shrinks).
        let avail = (ui.available_width() - 16.0).max(cell);
        let cols = ((avail + spacing) / (cell + spacing)).floor().max(1.0) as usize;
        app.set_grid_cols(cols);

        let len = app.visible_len();
        let rows = len.div_ceil(cols);
        // Virtualized: build only the rows scrolled into view. A folder with
        // thousands of images must not allocate every cell or load every
        // thumbnail (that exhausts memory and crashes).
        let reset_scroll = app.take_grid_scroll_reset();
        let mut grid_scroll = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("grid_scroll");
        if reset_scroll {
            grid_scroll = grid_scroll.scroll_offset(egui::vec2(0.0, 0.0));
        }
        grid_scroll.show_rows(ui, cell, rows, |ui, row_range| {
            let start = row_range.start * cols;
            let end = (row_range.end * cols).min(len);
            app.set_visible_grid_range(start, end);
            for row in row_range {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let pos = row * cols + col;
                        if pos < len {
                            grid_cell(ui, app, pos, cell, sel, out);
                        }
                    }
                });
            }
        });
    });
}

/// One folder row in the tree: an indent, a clickable disclosure glyph, and a
/// selectable folder name. Recurses into expanded folders' cached children.
pub(super) fn folder_node(ui: &mut egui::Ui, app: &App, path: &Path, depth: usize, out: &mut FrameOutput) {
    let selected = app.folder_sel().as_deref() == Some(path);
    let row = ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        // The disclosure glyph and name are a single selectable unit: one click
        // anywhere on the row opens the folder (load + toggle expansion).
        let glyph = if app.is_expanded(path) {
            "\u{25bc}"
        } else {
            "\u{25b6}"
        }; // ▼ / ▶
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        if ui
            .selectable_label(selected, format!("{glyph}  {name}"))
            .clicked()
        {
            out.actions.push(UiAction::OpenFolder(path.to_path_buf()));
        }
    });

    // Keyboard-focus outline around the selected row (single-select tree, so
    // this is the same folder as the blue selection above). Only shown while
    // the folder tree holds keyboard focus.
    if app.focus() == Region::Folders && selected {
        ui.painter().rect_stroke(
            row.response.rect,
            2.0,
            egui::Stroke::new(2.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Inside,
        );
    }

    if app.is_expanded(path) {
        // `app` is a shared (&App) borrow, so the recursive call can read the
        // same slice concurrently — no clone needed.
        for child in app.subdirs(path) {
            folder_node(ui, app, child, depth + 1, out);
        }
    }
}

/// The few visual/behavioral knobs that differ between a grid cell and a
/// filmstrip cell. Everything else about drawing a thumbnail cell is shared.
pub(super) struct CellStyle {
    /// Corner radius, also used as the thumbnail inset.
    corner: f32,
    /// Background gray level for an unselected cell.
    bg_gray: u8,
    /// Star label offset from the cell's bottom-left corner.
    star_dx: f32,
    star_dy: f32,
    /// Star label font size.
    star_size: f32,
    /// Draw a "…" placeholder while the thumbnail decodes (grid only).
    show_placeholder: bool,
    /// Omit the star label entirely when the rating is 0 (filmstrip only).
    hide_zero_stars: bool,
}

pub(super) const GRID_CELL_STYLE: CellStyle = CellStyle {
    corner: 4.0,
    bg_gray: 28,
    star_dx: 6.0,
    star_dy: -14.0,
    star_size: 13.0,
    show_placeholder: true,
    hide_zero_stars: false,
};

pub(super) const STRIP_CELL_STYLE: CellStyle = CellStyle {
    corner: 3.0,
    bg_gray: 24,
    star_dx: 4.0,
    star_dy: -8.0,
    star_size: 10.0,
    show_placeholder: false,
    hide_zero_stars: true,
};

/// Shared thumbnail cell for the grid and the filmstrip: paints background,
/// fitted thumbnail (or placeholder), star rating, and selection outline.
/// `selected` marks membership in the multi-selection; `primary` marks the
/// active cell (drawn with a brighter outline). Returns the response so callers
/// can add view-specific behavior (click routing, double-click, auto-scroll).
pub(super) fn thumbnail_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    selected: bool,
    primary: bool,
    style: &CellStyle,
) -> egui::Response {
    let size = egui::vec2(cell, cell);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    let bg = if selected || primary {
        theme::SELECTION_BG
    } else {
        egui::Color32::from_gray(style.bg_gray)
    };
    ui.painter().rect_filled(rect, style.corner, bg);

    if let Some((tex, tw, th)) = app.thumb_texture_for(pos) {
        let inner = rect.shrink(style.corner);
        let scale = (inner.width() / tw as f32).min(inner.height() / th as f32);
        let dw = tw as f32 * scale;
        let dh = th as f32 * scale;
        let img_rect = egui::Rect::from_center_size(inner.center(), egui::vec2(dw, dh));
        egui::Image::from_texture((tex.id(), egui::vec2(dw, dh))).paint_at(ui, img_rect);
    } else if style.show_placeholder {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{2026}",
            egui::FontId::proportional(18.0),
            egui::Color32::GRAY,
        );
    }

    // Best-of-burst overlay. Bursts imply no active filter, so every burst is
    // whole in the grid: the sharpest frame gets a badge, the rest are dimmed.
    match app.burst_mark_at(pos) {
        Some(BurstMark::Sibling) => {
            ui.painter()
                .rect_filled(rect, style.corner, egui::Color32::from_black_alpha(140));
        }
        Some(BurstMark::Best) => {
            // Top-left corner — rating stars live bottom-left, so no clash.
            let c = rect.left_top() + egui::vec2(style.corner + 9.0, style.corner + 9.0);
            ui.painter()
                .circle_filled(c, 9.0, egui::Color32::from_black_alpha(170));
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "\u{2605}",
                egui::FontId::proportional(13.0),
                theme::BURST_BADGE,
            );
        }
        None => {}
    }

    // Content-duplicate-group overlay. A photo can be in both a time-burst and
    // a content-duplicate group at once — these are separate underlying
    // computations, unified only here at the badge layer. Anchored at the
    // top-right corner so it never collides with the burst badge (top-left) or
    // the rating stars (bottom-left).
    match app.dup_mark_at(pos) {
        Some(DuplicateMark::Sibling) => {
            ui.painter()
                .rect_filled(rect, style.corner, egui::Color32::from_black_alpha(90));
        }
        Some(DuplicateMark::Best) => {
            let c = rect.right_top() + egui::vec2(-(style.corner + 9.0), style.corner + 9.0);
            ui.painter()
                .circle_filled(c, 9.0, egui::Color32::from_black_alpha(170));
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "D",
                egui::FontId::proportional(12.0),
                theme::DUP_BADGE,
            );
        }
        None => {}
    }

    let stars = app.rating_at(pos);
    if !(style.hide_zero_stars && stars == 0) {
        ui.painter().text(
            egui::pos2(rect.left() + style.star_dx, rect.bottom() + style.star_dy),
            egui::Align2::LEFT_CENTER,
            star_string(stars),
            egui::FontId::proportional(style.star_size),
            theme::STAR_GOLD,
        );
    }

    // Selection outline on top of the thumbnail: a brighter 3px stroke for the
    // primary/active cell, a thinner 2px stroke for other selected members.
    if selected || primary {
        let width = if primary { 3.0f32 } else { 2.0f32 };
        ui.painter().rect_stroke(
            rect,
            style.corner,
            egui::Stroke::new(width, theme::SELECTION_BLUE),
            egui::StrokeKind::Inside,
        );
    }

    response
}

/// One grid cell: a thumbnail image-button with selection highlight + stars.
pub(super) fn grid_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: Option<usize>,
    out: &mut FrameOutput,
) {
    // No outline in the grid's browse-first state (sel is None).
    let primary = sel == Some(pos);
    let selected = app.is_selected(pos);
    let response = thumbnail_cell(ui, app, pos, cell, selected, primary, &GRID_CELL_STYLE);
    if response.clicked() {
        // A click landing on the duplicate-group badge (top-right corner)
        // opens Survey Mode on that group instead of the normal select
        // behavior — same badge rect math as the one `thumbnail_cell` draws.
        if app.dup_mark_at(pos).is_some() {
            if let Some(click_pos) = response.interact_pointer_pos() {
                let badge_center = response.rect.right_top()
                    + egui::vec2(-(GRID_CELL_STYLE.corner + 9.0), GRID_CELL_STYLE.corner + 9.0);
                if click_pos.distance(badge_center) <= 10.0 {
                    out.actions.push(UiAction::OpenSurvey(pos));
                    out.actions.push(UiAction::Focus(Region::Grid));
                    return;
                }
            }
        }
        // Cmd toggles a cell, Shift extends the range, plain click selects one.
        let mods = ui.input(|i| i.modifiers);
        let action = if mods.shift {
            UiAction::SelectRange(pos)
        } else if mods.command {
            UiAction::SelectToggle(pos)
        } else {
            UiAction::Select(pos)
        };
        out.actions.push(action);
        out.actions.push(UiAction::Focus(Region::Grid));
    }
    if response.double_clicked() {
        out.actions.push(UiAction::OpenLoupe(pos));
    }
}
