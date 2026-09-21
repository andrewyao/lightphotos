use super::*;
use std::path::Path;

use super::info_panel::draw_info_panel;
use crate::app::GRID_CELL_PT;
use crate::app::{App, LeftTab, Region};
use crate::burst::BurstMark;
use crate::duplicates::DuplicateMark;

/// The left sidebar. A footer strip picks its tab: the folder tree, rooted at
/// the opened folder, or the focused photo's metadata.
pub(super) fn draw_left_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
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
            egui::Panel::bottom("left_tabs").show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    for tab in [LeftTab::Folders, LeftTab::Info] {
                        let resp = tab_icon(ui, tab, app.left_tab() == tab);
                        if resp.clicked() && app.left_tab() != tab {
                            out.actions.push(UiAction::SetLeftTab(tab));
                        }
                    }
                });
            });
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match app.left_tab() {
                    LeftTab::Folders => {
                        if let Some(root) = app.folder_root() {
                            folder_node(ui, app, &root, 0, out);
                        }
                    }
                    LeftTab::Info => draw_info_panel(ui, app),
                });
        });
}

/// One footer tab button, painted as an outline icon: a folder for the tree,
/// a page with a folded corner for the photo's info.
fn tab_icon(ui: &mut egui::Ui, tab: LeftTab, active: bool) -> egui::Response {
    let side = font_size::px(ui.style(), 26.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
    let visuals = ui.visuals();
    let color = if active {
        visuals.strong_text_color()
    } else if response.hovered() {
        visuals.text_color()
    } else {
        visuals.weak_text_color()
    };
    if active {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.selection.bg_fill.gamma_multiply(0.6));
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
    }
    let stroke = egui::Stroke::new(font_size::px(ui.style(), 1.3), color);
    let u = font_size::px(ui.style(), 1.0);
    let c = rect.center();
    let p = |x: f32, y: f32| egui::pos2(c.x + x * u, c.y + y * u);
    let outline = match tab {
        LeftTab::Folders => vec![
            p(-8.0, -5.5),
            p(-3.0, -5.5),
            p(-1.5, -3.5),
            p(8.0, -3.5),
            p(8.0, 6.0),
            p(-8.0, 6.0),
        ],
        LeftTab::Info => vec![
            p(-6.0, -8.0),
            p(2.5, -8.0),
            p(6.0, -4.5),
            p(6.0, 8.0),
            p(-6.0, 8.0),
        ],
    };
    ui.painter().add(egui::Shape::closed_line(outline, stroke));
    if tab == LeftTab::Info {
        ui.painter()
            .line(vec![p(2.5, -8.0), p(2.5, -4.5), p(6.0, -4.5)], stroke);
        for y in [0.0, 3.5] {
            ui.painter().line_segment([p(-3.0, y), p(3.0, y)], stroke);
        }
    }
    let tip = match tab {
        LeftTab::Folders => t().folders_tab_tip,
        LeftTab::Info => t().info_tab_tip,
    };
    response.on_hover_text(tip)
}

/// Width of the widest visible folder row, measured in the body font that
/// `selectable_label` uses. The sidebar sizes itself to this.
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
    let text_width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(name, font.clone(), egui::Color32::WHITE)
            .size()
            .x
    });
    let step = disclosure_w(ui.style());
    let own_width = depth as f32 * step + step + ui.spacing().item_spacing.x + text_width;
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
    let sel = app.sel();

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let cell = GRID_CELL_PT;
        let spacing = ui.spacing().item_spacing.x;
        // Leave room for the scrollbar, which narrows the scroll area's inner
        // width after `cols` is fixed.
        let avail = (ui.available_width() - 16.0).max(cell);
        let cols = ((avail + spacing) / (cell + spacing)).floor().max(1.0) as usize;
        app.set_grid_cols(cols);

        let len = app.visible_len();
        let rows = len.div_ceil(cols);
        // Build only the rows in view. Loading every thumbnail of a large
        // folder runs out of memory.
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

/// The disclosure triangle's width, which is also one level of folder indent.
fn disclosure_w(style: &egui::Style) -> f32 {
    font_size::px(style, 14.0)
}

/// Paint the folder row's disclosure triangle. It is drawn, not text, because
/// egui's built-in fonts have no ▼ glyph, and fonts that do have it place it
/// off the folder name's baseline.
fn disclosure_triangle(ui: &mut egui::Ui, expanded: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(disclosure_w(ui.style()), ui.spacing().interact_size.y),
        egui::Sense::click(),
    );
    let c = rect.center();
    let r = font_size::px(ui.style(), 4.0);
    let points = if expanded {
        vec![
            egui::pos2(c.x - r, c.y - r * 0.6),
            egui::pos2(c.x + r, c.y - r * 0.6),
            egui::pos2(c.x, c.y + r * 0.7),
        ]
    } else {
        vec![
            egui::pos2(c.x - r * 0.6, c.y - r),
            egui::pos2(c.x - r * 0.6, c.y + r),
            egui::pos2(c.x + r * 0.7, c.y),
        ]
    };
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        ui.visuals().text_color(),
        egui::Stroke::NONE,
    ));
    response
}

/// One folder row. Recurses into expanded folders.
pub(super) fn folder_node(
    ui: &mut egui::Ui,
    app: &App,
    path: &Path,
    depth: usize,
    out: &mut FrameOutput,
) {
    let selected = app.folder_sel().as_deref() == Some(path);
    let row = ui.horizontal(|ui| {
        ui.add_space(depth as f32 * disclosure_w(ui.style()));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let triangle = disclosure_triangle(ui, app.is_expanded(path));
        let label = ui.selectable_label(selected, name);
        if triangle.clicked() || label.clicked() {
            out.actions.push(UiAction::OpenFolder(path.to_path_buf()));
        }
    });

    if app.focus() == Region::Folders && selected {
        ui.painter().rect_stroke(
            row.response.rect,
            2.0,
            egui::Stroke::new(2.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Inside,
        );
    }

    if app.is_expanded(path) {
        for child in app.subdirs(path) {
            folder_node(ui, app, child, depth + 1, out);
        }
    }
}

/// What differs between a grid cell and a filmstrip cell.
pub(super) struct CellStyle {
    /// Corner radius, also used as the thumbnail inset.
    corner: f32,
    bg_gray: u8,
    /// Star label offset from the cell's bottom-left corner.
    star_dx: f32,
    star_dy: f32,
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

/// Radius of a corner badge, scaled with the UI text size.
fn badge_radius(style: &egui::Style) -> f32 {
    font_size::px(style, 9.0)
}

/// Centre of the corner badge `align` names in a cell of `rect`, inset past the
/// rounded corner.
fn badge_center(
    rect: egui::Rect,
    style: &CellStyle,
    badge_r: f32,
    align: egui::Align2,
) -> egui::Pos2 {
    align.pos_in_rect(&rect.shrink(style.corner + badge_r))
}

/// Thumbnail cell shared by the grid and the filmstrip. `selected` means the
/// cell is in the multi-selection; `primary` means it is the active cell and
/// gets a thicker outline.
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
        egui::Image::from_texture((tex, egui::vec2(dw, dh))).paint_at(ui, img_rect);
        #[cfg(target_arch = "wasm32")]
        if ui.is_rect_visible(img_rect) {
            crate::analytics::photo_drawn();
        }
    } else if style.show_placeholder {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{2026}",
            egui::FontId::proportional(font_size::px(ui.style(), 18.0)),
            egui::Color32::GRAY,
        );
    }

    let badge_r = badge_radius(ui.style());
    // Corners: burst badge top-left, duplicate badge top-right, eyes-closed
    // bottom-right, stars bottom-left. One photo can show all four.
    match app.burst_mark_at(pos) {
        Some(BurstMark::Sibling) => {
            ui.painter()
                .rect_filled(rect, style.corner, egui::Color32::from_black_alpha(140));
        }
        Some(BurstMark::Best) => {
            let c = badge_center(rect, style, badge_r, egui::Align2::LEFT_TOP);
            ui.painter()
                .circle_filled(c, badge_r, egui::Color32::from_black_alpha(170));
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "\u{2605}",
                egui::FontId::proportional(font_size::px(ui.style(), 13.0)),
                theme::BURST_BADGE,
            );
        }
        None => {}
    }

    match app.dup_mark_at(pos) {
        Some(DuplicateMark::Sibling) => {
            ui.painter()
                .rect_filled(rect, style.corner, egui::Color32::from_black_alpha(90));
        }
        Some(DuplicateMark::Best) => {
            let c = badge_center(rect, style, badge_r, egui::Align2::RIGHT_TOP);
            ui.painter()
                .circle_filled(c, badge_r, egui::Color32::from_black_alpha(170));
            ui.painter().text(
                c,
                egui::Align2::CENTER_CENTER,
                "D",
                egui::FontId::proportional(font_size::px(ui.style(), 12.0)),
                theme::DUP_BADGE,
            );
        }
        None => {}
    }

    if app.eyes_closed_at(pos) {
        let c = badge_center(rect, style, badge_r, egui::Align2::RIGHT_BOTTOM);
        ui.painter()
            .circle_filled(c, badge_r, egui::Color32::from_black_alpha(170));
        ui.painter().text(
            c,
            egui::Align2::CENTER_CENTER,
            // An arc, read as a closed eyelid.
            "\u{2312}",
            egui::FontId::proportional(font_size::px(ui.style(), 13.0)),
            theme::EYES_BADGE,
        );
    }

    // The color label fills the cell's bottom margin, below the thumbnail.
    if let Some(label) = app.label_at(pos) {
        let strip = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.bottom() - style.corner),
            rect.right_bottom(),
        );
        ui.painter().rect_filled(
            strip,
            egui::CornerRadius {
                sw: style.corner as u8,
                se: style.corner as u8,
                ..Default::default()
            },
            label_color(label),
        );
    }

    let stars = app.rating_at(pos);
    if !(style.hide_zero_stars && stars == 0) {
        ui.painter().text(
            egui::pos2(
                rect.left() + font_size::px(ui.style(), style.star_dx),
                rect.bottom() + font_size::px(ui.style(), style.star_dy),
            ),
            egui::Align2::LEFT_CENTER,
            star_string(stars),
            egui::FontId::proportional(font_size::px(ui.style(), style.star_size)),
            theme::STAR_GOLD,
        );
    }

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

pub(super) fn grid_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: Option<usize>,
    out: &mut FrameOutput,
) {
    let primary = sel == Some(pos);
    let selected = app.is_selected(pos);
    let response = thumbnail_cell(ui, app, pos, cell, selected, primary, &GRID_CELL_STYLE);
    if response.clicked() {
        // A click on the duplicate badge opens Survey Mode.
        if app.dup_mark_at(pos).is_some() {
            if let Some(click_pos) = response.interact_pointer_pos() {
                let badge_r = badge_radius(ui.style());
                let center = badge_center(
                    response.rect,
                    &GRID_CELL_STYLE,
                    badge_r,
                    egui::Align2::RIGHT_TOP,
                );
                // A hair of slop, so a click at the badge's edge still lands.
                if click_pos.distance(center) <= badge_r + 1.0 {
                    out.actions.push(UiAction::OpenSurvey(pos));
                    out.actions.push(UiAction::Focus(Region::Grid));
                    return;
                }
            }
        }
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
