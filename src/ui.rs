//! egui chrome: the thumbnail Grid, the Loupe filmstrip, the filter bar, and
//! rating overlays. The GPU renderer draws the loupe image itself; this module
//! draws everything around it and reports back the central image rect plus any
//! user actions (clicks, slider, filter changes) for `main.rs` to apply.
//!
//! Built against egui 0.34's `Panel`/`show_inside` API: `main.rs` runs us with
//! the root background `Ui` (from `Context::run_ui`), and we nest panels inside
//! it. The Loupe leaves its central region frameless/transparent so the wgpu
//! image shows through.

use std::path::Path;

use crate::develop::Adjustments;
use crate::navigation::Cmp;
use crate::app::{App, CropEdge, Region, ViewMode};

/// Shared palette. Several of these colors were previously duplicated as inline
/// `from_rgb(...)` literals across the grid and filmstrip cells; naming them
/// keeps the two views in sync.
mod theme {
    use egui::Color32;
    /// Star rating overlay (gold).
    pub const STAR_GOLD: Color32 = Color32::from_rgb(255, 210, 80);
    /// Selection outline on a thumbnail cell (blue).
    pub const SELECTION_BLUE: Color32 = Color32::from_rgb(90, 160, 255);
    /// Background tint behind the selected/active cell.
    pub const SELECTION_BG: Color32 = Color32::from_rgb(40, 80, 140);
    /// Keyboard-cursor outline (amber) — distinct from the blue mouse selection,
    /// used for the folder-tree cursor and the focused Develop slider.
    pub const CURSOR_AMBER: Color32 = Color32::from_rgb(255, 190, 90);
}

/// An action the UI wants `App` to perform after the frame is built. Positions
/// are indices into the *visible* list (same space as `App::sel`).
pub enum UiAction {
    /// Select the cell at this visible position (plain click / single select).
    Select(usize),
    /// Cmd-click: toggle this cell in the multi-selection.
    SelectToggle(usize),
    /// Shift-click: extend the range selection to this cell.
    SelectRange(usize),
    /// Open the loupe on this visible position.
    OpenLoupe(usize),
    /// Copy the primary photo's develop settings to the in-app clipboard.
    CopySettings,
    /// Ask to run a bulk action on the current selection (opens a confirm modal).
    RequestBulk(BulkKind),
    /// Confirm the pending bulk action.
    ConfirmBulk,
    /// Dismiss the pending bulk action without running it.
    CancelBulk,
    /// Set the thumbnail size (longest-side px).
    SetThumbPx(u32),
    /// Set (or clear) the star filter.
    SetFilter(Option<(Cmp, u8)>),
    /// Rate the current selection/shown image (0 clears).
    SetRating(u8),
    /// Open this folder as one unit: load its images and toggle its expansion.
    OpenFolder(std::path::PathBuf),
    /// Begin dragging this crop edge (pointer pressed near it).
    CropGrab(CropEdge),
    /// Begin moving the whole crop rectangle, anchored at this texture coordinate.
    CropGrabMove(f32, f32),
    /// Move the active crop drag to this normalized texture coordinate.
    CropDragTo(f32, f32),
    /// Release the active crop drag (drag ended).
    CropRelease,
    /// Set the develop adjustments for the current loupe image.
    SetAdjustments(Adjustments),
    /// Reset the current loupe image's develop adjustments to identity.
    ResetAdjustments,
    /// Give keyboard focus to this region (e.g. the user clicked into its panel).
    Focus(Region),
}

/// A bulk action requested from the toolbar, run against the current
/// multi-selection after confirmation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BulkKind {
    /// Set every selected photo's rating (0 clears it).
    Rate(u8),
    /// Export every selected photo as a baked JPG.
    Export,
    /// Apply the copied develop settings to every selected photo.
    ApplySettings,
    /// Move every selected photo to the Trash.
    Delete,
}

/// What `draw` returns to `main.rs` each frame.
#[derive(Default)]
pub struct FrameOutput {
    /// The central rect (logical px) left for the loupe image, in Loupe mode.
    pub loupe_rect: Option<egui::Rect>,
    /// Actions to apply this frame.
    pub actions: Vec<UiAction>,
}

/// Build the egui UI for one frame and return the loupe rect + actions.
pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();
    // Global toolbar first, so it reserves height above both modes (and, in the
    // loupe, before the frameless central rect is read).
    global_toolbar(ui, app, &mut out);
    match app.mode() {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
    }
    status_toast(ui, app);
    confirm_modal(ui, app, &mut out);
    out
}

/// A transient status message (e.g. an export result), shown bottom-center for a
/// few seconds. Requests a repaint so it disappears without further input.
fn status_toast(ui: &egui::Ui, app: &App) {
    let Some(text) = app.status_text() else { return };
    let screen = ui.ctx().content_rect();
    egui::Area::new(egui::Id::new("status_toast"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(screen.center().x, screen.max.y - 48.0))
        .pivot(egui::Align2::CENTER_CENTER)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .fill(egui::Color32::from_black_alpha(210))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(text).color(egui::Color32::WHITE));
                });
        });
    // Keep repainting until the toast expires so it clears on its own.
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
}

/// Stars as a compact string, e.g. 3 → "★★★☆☆".
fn star_string(stars: u8) -> String {
    let s = stars.min(5) as usize;
    let mut out = String::new();
    for _ in 0..s {
        out.push('\u{2605}'); // ★
    }
    for _ in s..5 {
        out.push('\u{2606}'); // ☆
    }
    out
}

/// The always-visible global toolbar, drawn once above both modes. Currently
/// hosts the rating filter (`All` + 5 stars → show photos rated ≥ N).
fn global_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("global_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            // The active `≥ N` threshold: `Some((Gte, v))` → v; anything else
            // (None, or the Unrated filter) lights no stars.
            let active = match app.filter() {
                Some((Cmp::Gte, v)) => v,
                _ => 0,
            };
            // `All` clears the filter; selected only when no filter is set.
            if ui.selectable_label(app.filter().is_none(), "All").clicked() {
                out.actions.push(UiAction::SetFilter(None));
            }
            ui.separator();
            // Five clickable stars: click star N → show ≥ N. Matches the
            // loupe rating overlay's glyphs/colors.
            for n in 1u8..=5 {
                let filled = n <= active;
                let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                let color = if filled {
                    theme::STAR_GOLD
                } else {
                    egui::Color32::from_gray(160)
                };
                let star = egui::Label::new(
                    egui::RichText::new(glyph).size(20.0).color(color),
                )
                .sense(egui::Sense::click());
                if ui
                    .add(star)
                    .on_hover_text(format!("Show photos rated \u{2265} {n}"))
                    .clicked()
                {
                    out.actions.push(UiAction::SetFilter(Some((Cmp::Gte, n))));
                }
            }
            ui.separator();
            // Show only unstarred photos (rating == 0), mutually exclusive with ≥N.
            let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
            if ui
                .selectable_label(unrated, "Unrated")
                .on_hover_text("Show only photos with no rating")
                .clicked()
            {
                // Toggle off back to All when it's already active.
                let next = if unrated { None } else { Some((Cmp::Eq, 0)) };
                out.actions.push(UiAction::SetFilter(next));
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
                // Copy the primary photo's settings; paste onto the whole selection.
                if ui
                    .button("Copy Settings")
                    .on_hover_text("Copy this photo's develop settings (Cmd+Shift+C)")
                    .clicked()
                {
                    out.actions.push(UiAction::CopySettings);
                }
                if ui
                    .add_enabled(app.has_copied_settings(), egui::Button::new("Apply Settings"))
                    .on_disabled_hover_text("Copy settings from a photo first")
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::ApplySettings));
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
        });
    });
}

/// A modal confirming a pending bulk action. Confirm runs it; Cancel / Esc /
/// clicking the backdrop dismisses it.
fn confirm_modal(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
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

/// Left folder-tree sidebar, rooted at the opened folder. Shown in both the grid
/// and the loupe so the folder structure is always visible.
fn draw_folders_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    if !app.folders_visible() {
        return;
    }
    egui::Panel::left("folders")
        .resizable(true)
        .default_size(220.0)
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(root) = app.folder_root() {
                    folder_node(ui, app, &root, 0, out);
                }
            });
        });
}

fn draw_grid(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
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
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, cell, rows, |ui, row_range| {
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
fn folder_node(ui: &mut egui::Ui, app: &App, path: &Path, depth: usize, out: &mut FrameOutput) {
    let row = ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        // The disclosure glyph and name are a single selectable unit: one click
        // anywhere on the row opens the folder (load + toggle expansion).
        let glyph = if app.is_expanded(path) { "\u{25bc}" } else { "\u{25b6}" }; // ▼ / ▶
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let selected = app.folder_sel().as_deref() == Some(path);
        if ui
            .selectable_label(selected, format!("{glyph}  {name}"))
            .clicked()
        {
            out.actions.push(UiAction::OpenFolder(path.to_path_buf()));
        }
    });

    // Keyboard cursor: an amber outline around the row, distinct from the blue
    // mouse selection. Only shown while the folder tree holds keyboard focus.
    if app.focus() == Region::Folders && app.folder_cursor().as_deref() == Some(path) {
        ui.painter().rect_stroke(
            row.response.rect,
            2.0,
            egui::Stroke::new(2.0, theme::CURSOR_AMBER),
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
struct CellStyle {
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

const GRID_CELL_STYLE: CellStyle = CellStyle {
    corner: 4.0,
    bg_gray: 28,
    star_dx: 6.0,
    star_dy: -14.0,
    star_size: 13.0,
    show_placeholder: true,
    hide_zero_stars: false,
};

const STRIP_CELL_STYLE: CellStyle = CellStyle {
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
fn thumbnail_cell(
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
        let width = if primary { 3.0 } else { 2.0 };
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
fn grid_cell(
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

fn draw_loupe(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let thumb_px = app.thumb_px();
    let sel = app.sel();
    // Only auto-scroll the strip to the selection when it actually changed.
    // Doing it every frame fights the user's clicks: the strip shifts between
    // press and release, so egui never registers the click.
    let follow = app.take_filmstrip_follow();

    // Left folder-tree sidebar (same as the grid) so structure stays visible.
    draw_folders_panel(ui, app, out);

    // The bottom filmstrip, unless hidden (Shift+Tab). When hidden, arrow keys
    // still step the photo — the strip is just the visual.
    if app.filmstrip_visible() {
        let strip_h = (thumb_px as f32 * 0.55).clamp(72.0, 200.0) + 8.0;
        egui::Panel::bottom("filmstrip")
            .exact_size(strip_h)
            .show_inside(ui, |ui| {
                let cell = strip_h - 16.0;
                let cell_full = cell + ui.spacing().item_spacing.x;
                let len = app.visible_len();

                // Virtualized horizontal strip (egui has no `show_columns`, so do
                // the grid's `show_rows` trick by hand): build only the cells in
                // view and report the range so loading tracks the scroll position.
                let mut area = egui::ScrollArea::horizontal().auto_shrink([false, false]);
                // On a selection change, center the selection for this frame only so
                // we don't fight the user's scrolling on other frames.
                if follow {
                    let sel = sel.unwrap_or(0) as f32;
                    let target =
                        (sel * cell_full + cell_full * 0.5 - ui.available_width() * 0.5).max(0.0);
                    area = area.scroll_offset(egui::vec2(target, 0.0));
                }
                area.show_viewport(ui, |ui, viewport| {
                    let first = (viewport.min.x / cell_full).floor().max(0.0) as usize;
                    let last = ((viewport.max.x / cell_full).ceil() as usize).min(len);
                    app.set_visible_strip_range(first, last);

                    ui.horizontal(|ui| {
                        // Leading + trailing spacers preserve the full content width
                        // so the scrollbar extent stays correct.
                        ui.add_space(first as f32 * cell_full);
                        for pos in first..last {
                            filmstrip_cell(ui, app, pos, cell, sel, out);
                        }
                        ui.add_space(len.saturating_sub(last) as f32 * cell_full);
                    });
                });
            });
    }

    // Right-hand develop panel (Temp/Tint/Exposure/… sliders + histogram). Drawn
    // before the central rect is read so it reserves its width first — otherwise
    // the wgpu image viewport would overlap the panel.
    if app.develop_visible() {
        draw_develop_panel(ui, app, out);
    }

    // Central region: deliberately NOT a CentralPanel. Leaving it as the root
    // UI's unused rect is what makes egui report the pointer there as "not over
    // egui" (`is_pointer_over_egui` checks `!root_ui_available_rect.contains`),
    // so scroll (zoom), clicks, and Space+drag (pan) reach the app. A
    // CentralPanel would consume the rect and egui would claim all pointer input
    // over the image, killing zoom/pan. The wgpu image is drawn into this rect.
    let central = ui.available_rect_before_wrap();
    out.loupe_rect = Some(central);

    if app.crop_rect().is_some() {
        // Crop mode: the crop overlay owns the whole central area (mask + edges).
        loupe_crop_overlay(ui, app, central, out);
    } else {
        // Star overlay in its own foreground Area, so egui owns clicks on the stars
        // (only there) without claiming the rest of the image area.
        loupe_star_overlay(ui, app, central, out);
    }
}

/// The crop-mode overlay: a dimmed mask outside the crop rectangle, a bright
/// outline with edge handles, and drag handling that moves whichever edge the
/// user grabs. The rectangle is stored in the app in texture space; here we map
/// it to screen via `App::loupe_tex_to_screen` (which accounts for zoom, pan and
/// rotation), so a grabbed screen edge maps back to the correct texture edge.
fn loupe_crop_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let Some(rect) = app.crop_rect() else { return };

    // The four texture-space edges as screen segments (endpoint pairs).
    let corner = |u, v| app.loupe_tex_to_screen(central, u, v);
    let tl = corner(rect.left, rect.top);
    let tr = corner(rect.right, rect.top);
    let bl = corner(rect.left, rect.bottom);
    let br = corner(rect.right, rect.bottom);
    let edges = [
        (CropEdge::Left, tl, bl),
        (CropEdge::Right, tr, br),
        (CropEdge::Top, tl, tr),
        (CropEdge::Bottom, bl, br),
    ];
    // Screen bounds of the crop (min/max copes with rotation flipping corners).
    let crop_screen = egui::Rect::from_points(&[tl, tr, bl, br]).intersect(central);

    egui::Area::new(egui::Id::new("loupe_crop"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::drag());
            let painter = ui.painter_at(central);

            // Dim the four bands around the crop rectangle.
            let dim = egui::Color32::from_black_alpha(150);
            let r = crop_screen;
            let full = central;
            let bands = [
                egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, r.min.y)), // top
                egui::Rect::from_min_max(egui::pos2(full.min.x, r.max.y), full.max), // bottom
                egui::Rect::from_min_max(egui::pos2(full.min.x, r.min.y), egui::pos2(r.min.x, r.max.y)), // left
                egui::Rect::from_min_max(egui::pos2(r.max.x, r.min.y), egui::pos2(full.max.x, r.max.y)), // right
            ];
            for b in bands {
                if b.is_positive() {
                    painter.rect_filled(b, 0.0, dim);
                }
            }

            // Crop outline + rule-of-thirds guides.
            let line = egui::Color32::from_gray(235);
            painter.rect_stroke(r, 0.0, egui::Stroke::new(1.5, line), egui::StrokeKind::Inside);
            for i in 1..3 {
                let fx = r.min.x + r.width() * i as f32 / 3.0;
                let fy = r.min.y + r.height() * i as f32 / 3.0;
                let faint = egui::Color32::from_white_alpha(70);
                painter.line_segment([egui::pos2(fx, r.min.y), egui::pos2(fx, r.max.y)], egui::Stroke::new(1.0, faint));
                painter.line_segment([egui::pos2(r.min.x, fy), egui::pos2(r.max.x, fy)], egui::Stroke::new(1.0, faint));
            }
            // Edge handles: a short bright bar at each edge midpoint.
            for (_, a, b) in edges {
                let mid = egui::pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                painter.circle_filled(mid, 5.0, line);
            }

            // Classify a pointer position: an edge (within grab threshold) takes
            // priority; otherwise inside the rectangle means "move the whole crop".
            const EDGE_GRAB_PX: f32 = 24.0;
            let inside_rect = |p: egui::Pos2| {
                let (u, v) = app.loupe_screen_to_tex(central, p);
                u >= rect.left && u <= rect.right && v >= rect.top && v <= rect.bottom
            };

            // Hover cursor hints: resize arrows on the edges, move icon inside.
            if let Some(p) = resp.hover_pos() {
                let icon = if let Some(edge) = nearest_edge(&edges, p, EDGE_GRAB_PX) {
                    match edge {
                        CropEdge::Left | CropEdge::Right => egui::CursorIcon::ResizeHorizontal,
                        CropEdge::Top | CropEdge::Bottom => egui::CursorIcon::ResizeVertical,
                    }
                } else if inside_rect(p) {
                    egui::CursorIcon::Move
                } else {
                    egui::CursorIcon::Default
                };
                ui.ctx().set_cursor_icon(icon);
            }

            // Drag handling: on press, grab the nearest edge (resize) or, if the
            // press is inside the rectangle, grab the whole rect (move). While
            // dragging, feed the pointer's texture coordinate to the active grab.
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    if let Some(edge) = nearest_edge(&edges, p, EDGE_GRAB_PX) {
                        out.actions.push(UiAction::CropGrab(edge));
                    } else if inside_rect(p) {
                        let (u, v) = app.loupe_screen_to_tex(central, p);
                        out.actions.push(UiAction::CropGrabMove(u, v));
                    }
                }
            }
            if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (u, v) = app.loupe_screen_to_tex(central, p);
                    out.actions.push(UiAction::CropDragTo(u, v));
                }
            }
            if resp.drag_stopped() {
                out.actions.push(UiAction::CropRelease);
            }
        });
}

/// The crop edge whose screen segment is nearest to `p`, if within `threshold`
/// px. Segments are `(edge, endpoint_a, endpoint_b)`.
fn nearest_edge(
    edges: &[(CropEdge, egui::Pos2, egui::Pos2)],
    p: egui::Pos2,
    threshold: f32,
) -> Option<CropEdge> {
    let mut best: Option<(CropEdge, f32)> = None;
    for &(edge, a, b) in edges {
        let d = dist_to_segment(p, a, b);
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((edge, d));
        }
    }
    best.filter(|&(_, d)| d <= threshold).map(|(e, _)| e)
}

/// Euclidean distance from point `p` to segment `a`–`b`.
fn dist_to_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    (p - proj).length()
}

/// The right-hand Develop panel: the Basic tone sliders, matching Lightroom's
/// order (WB → Tone → Highlights/Shadows/Whites/Blacks). Reads the current
/// image's adjustments from the app, and pushes `SetAdjustments` whenever a
/// slider actually changes (never every frame). A double-click on any slider
/// resets that one field to 0.
fn draw_develop_panel(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let mut adj = app.current_adjustments();

    egui::Panel::right("develop")
        .resizable(true)
        .default_size(340.0)
        .show_inside(ui, |ui| {
            draw_histogram(ui, app);
            ui.add_space(6.0);

            ui.horizontal(|ui| {
                ui.heading("Develop");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked() {
                        out.actions.push(UiAction::ResetAdjustments);
                        out.actions.push(UiAction::Focus(Region::Develop));
                    }
                });
            });
            ui.separator();

            // True once any slider in this frame changed, so we push exactly one
            // SetAdjustments after rendering the whole group. `interacted` tracks
            // mouse clicks/drags so we can move keyboard focus to the panel.
            let mut changed = false;
            let mut interacted = false;
            // Index of the slider being drawn, matched against `develop_focus` to
            // draw the keyboard-cursor outline. Advanced by every `slider(...)`.
            let mut idx = 0usize;
            let focus_idx = if app.focus() == Region::Develop {
                Some(app.develop_focus())
            } else {
                None
            };

            // One labeled slider over `field`. `focused` draws the amber keyboard
            // cursor. Returns (value changed, mouse-interacted).
            fn slider(
                ui: &mut egui::Ui,
                label: &str,
                field: &mut f32,
                range: std::ops::RangeInclusive<f32>,
                decimals: usize,
                focused: bool,
            ) -> (bool, bool) {
                ui.label(label);
                // Let the slider track fill the panel width, leaving room only
                // for the value box egui draws to its right. spacing is persistent
                // for the rest of the frame, so snapshot and restore it — otherwise
                // every widget drawn after the last slider inherits this width.
                let prev_width = ui.spacing().slider_width;
                ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(80.0);
                let resp = ui.add(
                    egui::Slider::new(field, range)
                        .max_decimals(decimals)
                        .show_value(true),
                );
                ui.spacing_mut().slider_width = prev_width;
                let mut changed = resp.changed();
                // Double-click the slider to reset this field to its default.
                if resp.double_clicked() {
                    *field = 0.0;
                    changed = true;
                }
                if focused {
                    ui.painter().rect_stroke(
                        resp.rect.expand(1.0),
                        2.0,
                        egui::Stroke::new(2.0, theme::CURSOR_AMBER),
                        egui::StrokeKind::Outside,
                    );
                }
                let interacted = resp.clicked() || resp.dragged() || resp.double_clicked();
                (changed, interacted)
            }

            // Render one slider: accumulate changed/interacted and bump `idx`.
            macro_rules! row {
                ($label:expr, $field:expr, $range:expr, $dec:expr) => {{
                    let (c, i) = slider(ui, $label, $field, $range, $dec, focus_idx == Some(idx));
                    changed |= c;
                    interacted |= i;
                    idx += 1;
                }};
            }

            ui.label(egui::RichText::new("White Balance").strong());
            row!("Temp", &mut adj.temp, crate::develop::TONE_RANGE, 0);
            row!("Tint", &mut adj.tint, crate::develop::TONE_RANGE, 0);
            ui.add_space(6.0);

            ui.label(egui::RichText::new("Tone").strong());
            row!("Exposure", &mut adj.exposure, crate::develop::EXPOSURE_RANGE, 2);
            row!("Contrast", &mut adj.contrast, crate::develop::TONE_RANGE, 0);
            row!("Highlights", &mut adj.highlights, crate::develop::TONE_RANGE, 0);
            row!("Shadows", &mut adj.shadows, crate::develop::TONE_RANGE, 0);
            row!("Whites", &mut adj.whites, crate::develop::TONE_RANGE, 0);
            row!("Blacks", &mut adj.blacks, crate::develop::TONE_RANGE, 0);
            let _ = idx; // final bump isn't read; silence unused-assignment

            if changed {
                out.actions.push(UiAction::SetAdjustments(adj));
            }
            if interacted {
                out.actions.push(UiAction::Focus(Region::Develop));
            }
        });
}

/// The live post-adjustment histogram at the top of the Develop panel. Draws a
/// dark frame, then the R/G/B channels as translucent filled curves (additive
/// overlap brightens) over a fixed-height rect. Reflects the current image's
/// adjustments because `App` re-bins `apply_linear`'d samples on every change.
fn draw_histogram(ui: &mut egui::Ui, app: &App) {
    let height = 120.0;
    let width = ui.available_width();
    let (rect, _resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    // Dark background frame.
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(16));
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(48)),
        egui::StrokeKind::Inside,
    );

    let Some(bins) = app.histogram() else { return };

    // Bins arrive float (fractional splat in recompute_histogram), so the comb
    // from re-quantizing a tone stretch is already gone. A single light box blur
    // tidies any residual gaps from strong stretches without flattening peaks —
    // giving Lightroom's smooth-but-detailed curve.
    let smooth = |ch: &[f32; 256]| -> [f32; 256] {
        let mut a = *ch;
        const R: usize = 1; // box radius
        let src = a;
        for i in 0..256usize {
            let lo = i.saturating_sub(R);
            let hi = (i + R).min(255);
            let mut sum = 0.0;
            for j in lo..=hi {
                sum += src[j];
            }
            a[i] = sum / (hi - lo + 1) as f32;
        }
        a
    };
    let smoothed: [[f32; 256]; 3] = [smooth(&bins[0]), smooth(&bins[1]), smooth(&bins[2])];

    // Shared max across all channels so relative channel heights stay honest.
    // Skip the extreme end bins (0 and 255) when scaling: pure black/white
    // clipping spikes would otherwise flatten everything else.
    let mut max = 1f32;
    for ch in &smoothed {
        for (i, &c) in ch.iter().enumerate() {
            if i == 0 || i == 255 {
                continue;
            }
            max = max.max(c);
        }
    }

    let colors = [
        egui::Color32::from_rgba_unmultiplied(255, 70, 70, 120),
        egui::Color32::from_rgba_unmultiplied(70, 255, 70, 120),
        egui::Color32::from_rgba_unmultiplied(90, 120, 255, 120),
    ];

    let x_at = |i: usize| rect.left() + (i as f32 / 255.0) * rect.width();
    let y_at = |count: f32| {
        let n = (count / max).min(1.0);
        rect.bottom() - n * rect.height()
    };

    for (ch, &color) in smoothed.iter().zip(colors.iter()) {
        // Each channel is a filled area curve: a triangle strip between the
        // baseline and the curve top. Translucent fills overlap to brighten,
        // giving the Lightroom additive look. A brighter polyline traces the top.
        let mut mesh = egui::Mesh::default();
        let base = rect.bottom();
        let mut top_line: Vec<egui::Pos2> = Vec::with_capacity(256);
        for (i, &count) in ch.iter().enumerate() {
            let x = x_at(i);
            let top = y_at(count);
            top_line.push(egui::pos2(x, top));
            let idx = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(x, base), color);
            mesh.colored_vertex(egui::pos2(x, top), color);
            if i > 0 {
                let p = idx - 2; // previous (base, top) pair
                mesh.add_triangle(p, p + 1, idx + 1);
                mesh.add_triangle(p, idx + 1, idx);
            }
        }
        painter.add(egui::Shape::mesh(mesh));
        // Crisper top edge.
        let line_color = color.to_opaque();
        painter.add(egui::Shape::line(
            top_line,
            egui::Stroke::new(1.0, line_color),
        ));
    }
}

/// Clickable 0–5 star rating overlay near the top of the loupe image.
/// Clicking the Nth star sets rating N; clicking the current rating clears it.
fn loupe_star_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let current = app.selected_rating();
    let star_w = 26.0;
    let total_w = star_w * 5.0;
    let left = central.center().x - total_w / 2.0;
    let top = central.top() + 10.0;

    // A foreground Area: egui claims pointer input over the stars (so a click
    // rates instead of starting a pan) but nowhere else in the image.
    egui::Area::new(egui::Id::new("loupe_stars"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(left, top))
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                for i in 0..5u8 {
                    let (rect, resp) = ui
                        .allocate_exact_size(egui::vec2(star_w, star_w), egui::Sense::click());
                    let filled = (i + 1) <= current;
                    let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                    let color = if filled {
                        theme::STAR_GOLD
                    } else {
                        egui::Color32::from_gray(160)
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        glyph,
                        egui::FontId::proportional(22.0),
                        color,
                    );
                    if resp.clicked() {
                        let n = i + 1;
                        // Clicking the current rating clears it (Lightroom behavior).
                        let stars = if n == current { 0 } else { n };
                        out.actions.push(UiAction::SetRating(stars));
                    }
                }
            });
        });
}

/// One filmstrip cell. Returns the response so the caller can auto-scroll.
fn filmstrip_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: Option<usize>,
    out: &mut FrameOutput,
) -> egui::Response {
    let primary = sel == Some(pos);
    let response = thumbnail_cell(ui, app, pos, cell, primary, primary, &STRIP_CELL_STYLE);
    if response.clicked() {
        out.actions.push(UiAction::Select(pos));
        out.actions.push(UiAction::Focus(Region::Filmstrip));
    }
    response
}
