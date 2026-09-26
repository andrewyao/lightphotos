// SPDX-License-Identifier: GPL-3.0-or-later

//! The one layout every form follows: sections, each a header over rows of a
//! weak label and its value, top aligned, with a divider and whitespace
//! between sections. Every size goes through `font_size::px`, so forms scale
//! with Alt+= and Alt+-.

use std::cell::Cell;

use super::font_size;

const HEADER: f32 = 15.0;
const HEADER_GAP: f32 = 4.0;
const ROW_GAP: f32 = 6.0;
const BEFORE_DIVIDER: f32 = 8.0;
const AFTER_DIVIDER: f32 = 12.0;

pub(super) struct Form {
    label_w: f32,
    first: Cell<bool>,
}

impl Form {
    /// A form whose label column fits the widest of `labels`.
    pub fn new(ui: &egui::Ui, labels: &[&str]) -> Self {
        let body = egui::TextStyle::Body.resolve(ui.style());
        let widest = labels
            .iter()
            .map(|label| {
                ui.fonts_mut(|f| {
                    f.layout_no_wrap(label.to_string(), body.clone(), egui::Color32::WHITE)
                        .size()
                        .x
                })
            })
            .fold(0.0, f32::max);
        Self {
            label_w: widest + ui.spacing().item_spacing.x * 2.0,
            first: Cell::new(true),
        }
    }

    /// A header over `body`'s rows, set off from the section before it. An
    /// empty `title` draws no header.
    pub fn section(&self, ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        if !self.first.replace(false) {
            ui.add_space(font_size::px(ui.style(), BEFORE_DIVIDER));
            ui.separator();
            ui.add_space(font_size::px(ui.style(), AFTER_DIVIDER));
        }
        if !title.is_empty() {
            let size = font_size::px(ui.style(), HEADER);
            ui.label(egui::RichText::new(title).size(size).strong());
            ui.add_space(font_size::px(ui.style(), HEADER_GAP));
        }
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = font_size::px(ui.style(), ROW_GAP);
            body(ui);
        });
    }

    /// `label` in the label column and `value` beside it, both from the row's
    /// top. Returns the label.
    pub fn row(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        value: impl FnOnce(&mut egui::Ui),
    ) -> egui::Response {
        ui.horizontal_top(|ui| {
            let label = ui
                .allocate_ui_with_layout(
                    egui::vec2(self.label_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(self.label_w);
                        ui.label(egui::RichText::new(label).weak())
                    },
                )
                .inner;
            ui.vertical(|ui| {
                // Controls stacked inside one value keep egui's own spacing;
                // the wider gap is for between rows.
                ui.spacing_mut().item_spacing.y = ui.ctx().global_style().spacing.item_spacing.y;
                value(ui)
            });
            label
        })
        .inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Seen {
        labels: Vec<egui::Rect>,
        values: Vec<egui::Rect>,
    }

    /// Two sections of two rows, each value two lines tall.
    fn draw() -> Seen {
        let mut seen = Seen::default();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let labels = ["Short", "A much longer label"];
            let form = Form::new(ui, &labels);
            for title in ["First", "Second"] {
                form.section(ui, title, |ui| {
                    for label in labels {
                        let l = form.row(ui, label, |ui| {
                            seen.values.push(ui.label("line one\nline two").rect);
                        });
                        seen.labels.push(l.rect);
                    }
                });
            }
        });
        seen
    }

    #[test]
    fn rows_share_a_label_column_and_align_to_their_tops() {
        let seen = draw();
        for (l, v) in seen.labels.iter().zip(&seen.values) {
            assert_eq!(l.left(), seen.labels[0].left(), "labels share one column");
            assert_eq!(v.left(), seen.values[0].left(), "values share one column");
            assert_eq!(
                l.top(),
                v.top(),
                "a two-line value starts level with its label"
            );
        }
        let widest = seen.labels.iter().map(|l| l.right()).fold(0.0, f32::max);
        assert!(
            seen.values[0].left() > widest,
            "values sit past the widest label"
        );
    }

    #[test]
    fn sections_are_divided_and_spaced() {
        let seen = draw();
        let first_end = seen.values[1].bottom();
        let second_row = seen.labels[2].top();
        assert!(
            second_row - first_end > BEFORE_DIVIDER + AFTER_DIVIDER + HEADER,
            "the next section's first row clears the divider, the gap and its header: {first_end} -> {second_row}"
        );
        let in_section = seen.labels[1].top() - seen.values[0].bottom();
        assert!(
            in_section >= ROW_GAP - 0.5,
            "rows are {ROW_GAP}px apart, got {in_section}"
        );
    }
}
