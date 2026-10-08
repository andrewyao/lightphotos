//! The home page, shown when no folder is open. It draws the Grid's own
//! layout with nothing in it, so the window looks the way it will once a
//! folder opens and the tour can point at each region: the left panel waits
//! for the folder tree, the toolbars and rail are switched off, and the
//! content area is a field of empty tiles under the prompt and the Open
//! Folder and Reopen Session buttons.

use super::*;

/// Panels claim space in the order `ui::draw` claims them for the Grid, so
/// every region sits where it will once a folder is open.
pub(super) fn draw_home(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    left_panel(ui);
    draw_develop_rail(ui, app, false, out);
    toolbar::placeholder_toolbar(ui, app);
    toolbar::placeholder_selection_bar(ui);
    let open = content(ui, app, out);
    allow_note(ui, app, open, out);
    status_toast(ui, app);
    help_modal(ui, app, out);
    settings_modal(ui, app, out);
}

/// The left panel's width, the Grid's minimum for its folder tree.
const PANEL_WIDTH: f32 = 220.0;
const PANEL_MARGIN: f32 = 12.0;

/// "Folders" over a hint at the tree that will fill it.
fn left_panel(ui: &mut egui::Ui) {
    let width = font_size::px(ui.style(), PANEL_WIDTH);
    let panel = egui::Panel::left("folders")
        .resizable(false)
        .exact_size(width)
        .show_inside(ui, |ui| {
            let margin = font_size::px(ui.style(), PANEL_MARGIN);
            ui.add_space(margin);
            ui.label(egui::RichText::new(t().folders_heading).strong());
            ui.add_space(margin);
            ui.weak(t().folder_tree_hint);
        });
    tour::anchor(ui.ctx(), tour::TourStep::Folders, panel.response.rect);
}

const BUTTON_WIDTH: f32 = 220.0;
const BUTTON_HEIGHT: f32 = 52.0;
const BUTTON_GAP: f32 = 12.0;

/// Open Folder, with Reopen Session beside it once a folder has been opened
/// before, centred as a row. Returns Open Folder's rect, for the note that
/// points at it.
fn buttons(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) -> egui::Rect {
    let style = ui.style().clone();
    let size = egui::vec2(
        font_size::px(&style, BUTTON_WIDTH),
        font_size::px(&style, BUTTON_HEIGHT),
    );
    let gap = font_size::px(&style, BUTTON_GAP);
    let session = app.saved_session();
    let row_w = if session.is_some() {
        2.0 * size.x + gap
    } else {
        size.x
    };
    ui.allocate_ui_with_layout(
        egui::vec2(row_w, size.y),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let open = open_folder_button(ui, app, size, out);
            if let Some(session) = session {
                let reopen = ui
                    .add_enabled(
                        !app.folder_pick_pending(),
                        egui::Button::new(
                            egui::RichText::new(t().reopen_session)
                                .size(font_size::px(&style, 18.0)),
                        )
                        .corner_radius(10.0)
                        .min_size(size),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text((t().reopen_session_tip)(
                        &session.root.display().to_string(),
                    ));
                if reopen.clicked() {
                    out.actions.push(UiAction::ReopenSession);
                }
            }
            open
        },
    )
    .inner
}

/// The home page's one call to action, filled in the brand blue so it reads
/// as the thing to press.
fn open_folder_button(
    ui: &mut egui::Ui,
    app: &App,
    size: egui::Vec2,
    out: &mut FrameOutput,
) -> egui::Rect {
    let pending = app.folder_pick_pending();
    let label = if pending {
        t().opening
    } else {
        t().open_folder
    };
    let resp = ui.add_enabled(
        !pending,
        egui::Button::new(
            egui::RichText::new(label)
                .size(font_size::px(ui.style(), 20.0))
                .color(egui::Color32::WHITE)
                .strong(),
        )
        .fill(theme::BRAND_BLUE)
        .corner_radius(10.0)
        .min_size(size),
    );
    if resp.hovered() && !pending {
        // A fixed fill gives no hover feedback of its own.
        ui.painter().rect_stroke(
            resp.rect.expand(2.0),
            12.0,
            egui::Stroke::new(2.0_f32, theme::BRAND_BLUE.linear_multiply(0.5)),
            egui::StrokeKind::Outside,
        );
    }
    let rect = resp.rect;
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        out.actions.push(UiAction::PickFolder);
    }
    rect
}

const TILE: f32 = crate::app::GRID_CELL_PT;
/// How far the overlay darkens the placeholder layout behind the card, and
/// the card's widest.
const OVERLAY_ALPHA: u8 = 150;
const CARD_WIDTH: f32 = 820.0;
/// The room inside the card, sides and top-bottom.
const CARD_PAD: egui::Vec2 = egui::vec2(72.0, 64.0);
const TILE_GAP: f32 = 8.0;

/// Rows of empty tiles where the thumbnails will go, the whole layout
/// dimmed, and a card over it holding the prompt, the buttons and a Take the
/// Tour link. Returns Open Folder's rect.
fn content(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) -> egui::Rect {
    let pal = theme::colors(ui.ctx());
    let tile_fill = pal.panel.lerp_to_gamma(pal.value, 0.05);
    let panel = egui::CentralPanel::default().show_inside(ui, |ui| {
        let area = ui.max_rect();
        let painter = ui.painter_at(area);
        let step = TILE + TILE_GAP;
        let cols = ((area.width() + TILE_GAP) / step).floor().max(1.0) as usize;
        let rows = ((area.height() + TILE_GAP) / step).ceil() as usize;
        for row in 0..rows {
            for col in 0..cols {
                let min = area.min + egui::vec2(col as f32 * step, row as f32 * step);
                painter.rect_filled(
                    egui::Rect::from_min_size(min, egui::vec2(TILE, TILE)),
                    6.0,
                    tile_fill,
                );
            }
        }

        // One dim layer over every placeholder region below the header, so
        // the layout reads as a preview behind the card. It paints on the
        // panels' own layer after them and before the card's widgets.
        let screen = ui.ctx().content_rect();
        let top =
            tour::anchored(ui.ctx(), tour::TourStep::Header).map_or(screen.min.y, |h| h.max.y);
        ui.ctx()
            .layer_painter(egui::LayerId::background())
            .rect_filled(
                egui::Rect::from_min_max(egui::pos2(screen.min.x, top), screen.max),
                0.0,
                egui::Color32::from_black_alpha(OVERLAY_ALPHA),
            );

        let prompt = egui::RichText::new(t().landing_prompt)
            .size(font_size::px(ui.style(), 32.0))
            .color(pal.value)
            .strong();
        let tagline = egui::RichText::new(t().landing_tagline)
            .size(font_size::px(ui.style(), 19.0))
            .color(pal.label);
        let width = (area.width() - 32.0)
            .min(font_size::px(ui.style(), CARD_WIDTH))
            .max(120.0);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_center_size(
                    area.center(),
                    egui::vec2(width, font_size::px(ui.style(), 420.0)),
                ))
                .layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| {
                egui::Frame::popup(ui.style())
                    .fill(pal.panel)
                    .corner_radius(14.0)
                    .inner_margin(egui::Margin::symmetric(
                        font_size::px(ui.style(), CARD_PAD.x).min(127.0) as i8,
                        font_size::px(ui.style(), CARD_PAD.y).min(127.0) as i8,
                    ))
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(prompt).wrap());
                            ui.add_space(font_size::px(ui.style(), 10.0));
                            ui.add(egui::Label::new(tagline).wrap());
                            ui.add_space(font_size::px(ui.style(), 40.0));
                            let open = buttons(ui, app, out);
                            ui.add_space(font_size::px(ui.style(), 20.0));
                            if ui.link(t().take_tour).on_hover_text(t().tour_tip).clicked() {
                                out.actions.push(UiAction::StartTour);
                            }
                            open
                        })
                        .inner
                    })
                    .inner
            },
        )
        .inner
    });
    tour::anchor(ui.ctx(), tour::TourStep::Content, panel.response.rect);
    panel.inner
}

/// Web only: picking a folder hands the browser a File System Access
/// permission, so a note under Open Folder says up front which button to
/// press. It is open each time the home page shows until its × closes it,
/// and steps aside while the tour runs.
fn allow_note(ui: &egui::Ui, app: &App, open: egui::Rect, out: &mut FrameOutput) {
    let note = t().landing_allow_note;
    if note.is_empty() || !app.allow_note_open() || app.tour_step().is_some() {
        return;
    }
    let ctx = ui.ctx();
    let arrow = font_size::px(ui.style(), 10.0);
    let fill = theme::colors(ctx)
        .panel
        .lerp_to_gamma(theme::BRAND_BLUE, 0.12);
    let stroke = egui::Stroke::new(1.0_f32, theme::BRAND_BLUE.linear_multiply(0.6));
    let max_w = font_size::px(ui.style(), 420.0);
    let shown = egui::Area::new(egui::Id::new("allow_note"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(open.min.x, open.max.y + 2.0 * arrow))
        .constrain_to(ctx.content_rect())
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(fill)
                .stroke(stroke)
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.set_max_width(max_w);
                    ui.horizontal_top(|ui| {
                        ui.add(egui::Label::new(note).wrap_mode(egui::TextWrapMode::Wrap));
                        if ui
                            .small_button("\u{d7}")
                            .on_hover_text(t().close_note_tip)
                            .clicked()
                        {
                            out.actions.push(UiAction::CloseAllowNote);
                        }
                    });
                });
        });
    // A small triangle on the note's top edge, pointing up at the button.
    let note_rect = shown.response.rect;
    let tip_x = open
        .center()
        .x
        .clamp(note_rect.min.x + arrow * 1.5, note_rect.max.x - arrow * 1.5);
    let top = note_rect.min.y;
    let painter = ctx.layer_painter(shown.response.layer_id);
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(tip_x - arrow, top + 1.0),
            egui::pos2(tip_x, top - arrow),
            egui::pos2(tip_x + arrow, top + 1.0),
        ],
        fill,
        egui::Stroke::NONE,
    ));
    painter.line_segment(
        [
            egui::pos2(tip_x - arrow, top),
            egui::pos2(tip_x, top - arrow),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(tip_x, top - arrow),
            egui::pos2(tip_x + arrow, top),
        ],
        stroke,
    );
}
