use super::*;
use std::path::Path;

use super::info_panel::draw_info_panel;
use crate::app::GRID_CELL_PT;
use crate::app::{App, LeftTab, Region};

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
            let t = t();
            let tabs = [
                (LeftTab::Folders, t.browse_tab, t.folders_tab_tip),
                (LeftTab::Info, t.metadata_tab, t.info_tab_tip),
            ];
            if let Some(tab) = super::tabs::footer(ui, "left_tabs", &tabs, app.left_tab()) {
                out.actions.push(UiAction::SetLeftTab(tab));
            }
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

/// Width of the widest visible folder row, measured in the body font that
/// `selectable_label` uses. The sidebar sizes itself to this.
fn folder_content_width(
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
        if len == 0 && (app.filter().is_some() || app.eyes_filter_on()) {
            no_matches(ui, app, out);
            return;
        }
        let rows = len.div_ceil(cols);
        // Build only the rows in view. Loading every thumbnail of a large
        // folder runs out of memory.
        let reset_scroll = app.take_grid_scroll_reset();
        let mut grid_scroll = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("grid_scroll");
        // Last frame's scroll offset and viewport height, for keeping a newly
        // selected photo in view.
        let viewport_id = egui::Id::new("grid_viewport");
        let (mut offset, view_h) = ui
            .ctx()
            .data(|d| d.get_temp::<(f32, f32)>(viewport_id))
            .unwrap_or((0.0, 0.0));
        if reset_scroll {
            offset = 0.0;
            grid_scroll = grid_scroll.scroll_offset(egui::vec2(0.0, 0.0));
        }
        // Scroll only when the selection changes, and only as far as brings
        // its row into view, so it doesn't fight manual scrolling.
        if let Some(sel) = sel.filter(|_| view_h > 0.0) {
            let last_sel_id = egui::Id::new("grid_last_sel");
            let prev_sel = ui.ctx().data(|d| d.get_temp::<usize>(last_sel_id));
            ui.ctx().data_mut(|d| d.insert_temp(last_sel_id, sel));
            if prev_sel != Some(sel) {
                let row_h = cell + ui.spacing().item_spacing.y;
                let top = (sel / cols) as f32 * row_h;
                let bottom = top + cell;
                let target = if top < offset {
                    Some(top)
                } else if bottom > offset + view_h {
                    Some(bottom - view_h)
                } else {
                    None
                };
                if let Some(target) = target {
                    grid_scroll = grid_scroll.scroll_offset(egui::vec2(0.0, target.max(0.0)));
                }
            }
        }
        app.clear_grid_cells();
        let output = grid_scroll.show_rows(ui, cell, rows, |ui, row_range| {
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
        ui.ctx().data_mut(|d| {
            d.insert_temp(
                viewport_id,
                (output.state.offset.y, output.inner_rect.height()),
            )
        });
    });
}

/// Stands in for a grid the filters emptied, so it doesn't read as a folder
/// with no photos.
fn no_matches(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = crate::i18n::t();
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.35);
        ui.label(egui::RichText::new(t.no_filter_matches).size(font_size::px(ui.style(), 16.0)));
        ui.add_space(8.0);
        if ui.button(t.show_all_photos).clicked() {
            out.actions.push(UiAction::SetFilter(None));
            if app.eyes_filter_on() {
                out.actions.push(UiAction::ToggleEyesClosed);
            }
        }
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
fn folder_node(ui: &mut egui::Ui, app: &App, path: &Path, depth: usize, out: &mut FrameOutput) {
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
            egui::Stroke::new(2.0f32, theme::colors(ui.ctx()).cursor),
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
    /// The filmstrip's cells sit a shade darker than the grid's.
    strip: bool,
    rating: RatingMark,
    /// Draw a "…" placeholder while the thumbnail decodes (grid only).
    show_placeholder: bool,
}

/// How a cell shows its star rating.
enum RatingMark {
    /// A "★★★☆☆" label, shown even at 0, offset from the cell's bottom-left
    /// corner.
    Stars { dx: f32, dy: f32, size: f32 },
    /// One dot per star, sized with the cell and absent at 0. Star glyphs are
    /// illegible at filmstrip size.
    Dots,
}

pub(super) const GRID_CELL_STYLE: CellStyle = CellStyle {
    corner: 4.0,
    strip: false,
    rating: RatingMark::Stars {
        dx: 6.0,
        dy: -14.0,
        size: 13.0,
    },
    show_placeholder: true,
};

pub(super) const STRIP_CELL_STYLE: CellStyle = CellStyle {
    corner: 3.0,
    strip: true,
    rating: RatingMark::Dots,
    show_placeholder: false,
};

const PILL_CORNER: egui::Align2 = egui::Align2::RIGHT_TOP;

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

    let colors = theme::colors(ui.ctx());
    let marked = selected || primary;
    let bg = match (style.strip, marked) {
        (true, false) => colors.strip_cell,
        (true, true) => colors.strip_cell_selected,
        (false, false) => colors.grid_cell,
        (false, true) => colors.grid_cell_selected,
    };
    ui.painter().rect_filled(rect, style.corner, bg);

    // A selected photo steps back from the cell edge, leaving a margin inside
    // its outline.
    let margin = if marked {
        font_size::px(ui.style(), 5.0)
    } else {
        0.0
    };

    let members = app.group_at(pos).map(|(_, g)| g.members().len());
    let card_step = font_size::px(ui.style(), 3.0);
    let cards = members.is_some() && !style.strip;
    let mut inner = rect.shrink(style.corner + margin);
    if cards {
        inner.min.y += 2.0 * card_step;
        inner.max.x -= 2.0 * card_step;
    }
    let mut pill_anchor = inner;

    if let Some((tex, tw, th)) = app.thumb_texture_for(pos) {
        let scale = (inner.width() / tw as f32).min(inner.height() / th as f32);
        let dw = tw as f32 * scale;
        let dh = th as f32 * scale;
        let img_rect = egui::Rect::from_center_size(inner.center(), egui::vec2(dw, dh));
        if cards {
            for k in [2.0, 1.0] {
                let card = img_rect.translate(egui::vec2(k * card_step, -k * card_step));
                ui.painter().rect(
                    card,
                    style.corner,
                    colors.stack_card,
                    egui::Stroke::new(1.0_f32, bg),
                    egui::StrokeKind::Inside,
                );
            }
        }
        egui::Image::from_texture((tex, egui::vec2(dw, dh))).paint_at(ui, img_rect);
        pill_anchor = img_rect;
        #[cfg(target_arch = "wasm32")]
        if ui.is_rect_visible(img_rect) {
            crate::analytics::photo_drawn();
        }
    } else if style.show_placeholder {
        // A failed decode must not look like one still loading.
        let (text, size, color) = if app.thumb_failed_at(pos) {
            (crate::i18n::t().thumb_unreadable, 12.0, colors.danger)
        } else {
            ("\u{2026}", 18.0, colors.label)
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(font_size::px(ui.style(), size)),
            color,
        );
    }

    if let Some(n) = members {
        count_pill(ui, pill_anchor, rect, n, &colors);
    }

    let badge_r = badge_radius(ui.style());
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
    match style.rating {
        RatingMark::Stars { dx, dy, size } => {
            let font = egui::FontId::proportional(font_size::px(ui.style(), size));
            let drawn = ui.painter().text(
                egui::pos2(
                    rect.left() + font_size::px(ui.style(), dx),
                    rect.bottom() + font_size::px(ui.style(), dy),
                ),
                egui::Align2::LEFT_CENTER,
                star_string(stars),
                font.clone(),
                colors.star,
            );
            if let Some((value, stale)) = app.score_at(pos) {
                ui.painter().text(
                    egui::pos2(
                        drawn.right() + font_size::px(ui.style(), 6.0),
                        drawn.center().y,
                    ),
                    egui::Align2::LEFT_CENTER,
                    value.to_string(),
                    font,
                    score_color(&colors, stale),
                );
            }
        }
        RatingMark::Dots => {
            let r = (cell * 0.028).max(2.0);
            let step = r * 2.8;
            let first =
                rect.left_bottom() + egui::vec2(style.corner + r + 1.0, -(style.corner + r + 1.0));
            for i in 0..stars {
                ui.painter().circle(
                    first + egui::vec2(i as f32 * step, 0.0),
                    r,
                    colors.star,
                    egui::Stroke::new(1.0_f32, egui::Color32::from_black_alpha(160)),
                );
            }
        }
    }

    if marked {
        let width = if primary { 1.5f32 } else { 1.0f32 };
        ui.painter().rect_stroke(
            rect,
            style.corner,
            egui::Stroke::new(width, colors.selection),
            egui::StrokeKind::Inside,
        );
        // Filmstrip cells are too small to carry a check as well.
        if !style.strip {
            let check_r = badge_r * 0.7;
            selection_check(
                ui,
                badge_center(rect, style, check_r, egui::Align2::LEFT_TOP),
                check_r,
                colors.selection,
            );
        }
    }

    response
}

/// A stale score, one whose photo was edited after scoring, is dimmed.
pub(super) fn score_color(colors: &theme::Palette, stale: bool) -> egui::Color32 {
    if stale {
        colors.label.gamma_multiply(0.45)
    } else {
        colors.label
    }
}

fn count_pill(
    ui: &egui::Ui,
    anchor: egui::Rect,
    cell: egui::Rect,
    count: usize,
    colors: &theme::Palette,
) {
    let font = egui::FontId::proportional(font_size::px(ui.style(), 11.0));
    let galley = ui
        .painter()
        .layout_no_wrap(count.to_string(), font, colors.pill_text);
    let pad = egui::vec2(
        font_size::px(ui.style(), 5.0),
        font_size::px(ui.style(), 1.5),
    );
    let size = galley.size() + 2.0 * pad;
    let size = egui::vec2(size.x.max(size.y), size.y);
    let inset = font_size::px(ui.style(), 4.0);
    let pill = PILL_CORNER.align_size_within_rect(size, anchor.shrink(inset));
    let pill = pill.translate(egui::vec2(
        (cell.left() + inset - pill.left()).max(0.0),
        0.0,
    ));
    ui.painter()
        .rect_filled(pill, size.y / 2.0, colors.pill_fill);
    ui.painter().galley(
        pill.center() - galley.size() / 2.0,
        galley,
        colors.pill_text,
    );
}

/// A blue disc with a white check, drawn as strokes so it needs no glyph from
/// the bundled fonts.
fn selection_check(ui: &egui::Ui, c: egui::Pos2, r: f32, fill: egui::Color32) {
    let painter = ui.painter();
    painter.circle(c, r, fill, egui::Stroke::new(1.0_f32, egui::Color32::WHITE));
    let stroke = egui::Stroke::new((r * 0.24).max(1.2), egui::Color32::WHITE);
    let a = c + egui::vec2(-0.45 * r, 0.02 * r);
    let b = c + egui::vec2(-0.12 * r, 0.35 * r);
    let d = c + egui::vec2(0.45 * r, -0.3 * r);
    painter.line_segment([a, b], stroke);
    painter.line_segment([b, d], stroke);
}

fn grid_cell(
    ui: &mut egui::Ui,
    app: &mut App,
    pos: usize,
    cell: f32,
    sel: Option<usize>,
    out: &mut FrameOutput,
) {
    let primary = sel == Some(pos);
    let selected = app.is_selected(pos);
    let response = thumbnail_cell(ui, app, pos, cell, selected, primary, &GRID_CELL_STYLE);
    app.record_grid_cell(pos, response.rect);
    if response.clicked() {
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
