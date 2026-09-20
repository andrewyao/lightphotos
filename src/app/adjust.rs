use super::*;
use std::path::Path;

use crate::develop::{self, Adjustments, GpuAdjust, GpuTouchUp, TouchUp};
use crate::image_ops;

impl App {
    /// Develop adjustments of the image currently shown (identity if unset).
    pub(crate) fn current_adjustments(&self) -> Adjustments {
        self.shown
            .path()
            .and_then(|p| self.edits.get(p))
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn current_touchups(&self) -> &[TouchUp] {
        self.shown
            .path()
            .and_then(|p| self.touchups.get(p))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn touchup_active(&self) -> bool {
        self.tool == LoupeTool::TouchUp
    }
    pub(crate) fn touchup_radius(&self) -> f32 {
        self.touchup_radius.max(self.touchup_radius_min())
    }
    pub(crate) fn touchup_selected(&self) -> Option<usize> {
        self.touchup_selected
    }
    pub(crate) fn set_touchup_radius(&mut self, radius: f32) {
        self.touchup_radius = radius.clamp(self.touchup_radius_min(), TOUCHUP_MAX_RADIUS);
    }
    pub(crate) fn touchup_radius_min(&self) -> f32 {
        let (w, h) = self.image_size();
        (TOUCHUP_MIN_PIXELS / w.min(h)).min(TOUCHUP_MAX_RADIUS)
    }

    /// Touch-up radius in normalized UV units for each image axis. The stored
    /// radius is relative to the source image's shorter dimension.
    pub(crate) fn touchup_uv_radii(&self, radius: f32) -> (f32, f32) {
        let (w, h) = self.image_size();
        let min_dim = w.min(h);
        (radius * min_dim / w, radius * min_dim / h)
    }

    /// Save `adj` for the shown image and push it to the GPU. Identity edits are
    /// removed from the edits map rather than stored.
    pub(super) fn apply_adjustments(&mut self, adj: Adjustments) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if adj.is_identity() {
            self.edits.remove(&path);
        } else {
            self.edits.insert(path.clone(), adj);
        }
        if self.unsaved_edit.as_ref().is_some_and(|p| *p != path) {
            self.save_edit();
        }
        self.unsaved_edit = Some(path);
        self.save_edit_unless_dragging();
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    /// A slider drag changes the edit every frame. Writing the sidecar each
    /// time blocks the UI thread on disk I/O, so wait for the mouse release.
    pub(crate) fn save_edit_unless_dragging(&mut self) {
        if !self.egui_ctx.input(|i| i.pointer.any_down()) {
            self.save_edit();
        }
    }

    pub(crate) fn save_edit(&mut self) {
        if let Some(path) = self.unsaved_edit.take() {
            let adj = self.edits.get(&path).copied().unwrap_or_default();
            self.catalog.set_adjustments(&path, &adj);
        }
    }

    /// Push the current image's adjustments to the renderer. Call it whenever
    /// the shown image or its edits change.
    pub(super) fn push_adjustments(&mut self) {
        let gpu = self.gpu_adjust(&self.current_adjustments());
        let gpu_touchups: Vec<GpuTouchUp> = self
            .current_touchups()
            .iter()
            .map(GpuTouchUp::from)
            .collect();
        if let Some(r) = &mut self.renderer {
            r.set_adjustments(gpu);
            r.set_touchups(&gpu_touchups);
        }
        self.request_redraw();
    }

    /// Convert `adj` to the GPU uniform. Also fills `texel_w`/`texel_h` from the
    /// image size, which denoise needs to step by whole texels.
    pub(super) fn gpu_adjust(&self, adj: &Adjustments) -> GpuAdjust {
        let (w, h) = self.image_size();
        let mut g = GpuAdjust::from(adj);
        g.texel_w = 1.0 / w;
        g.texel_h = 1.0 / h;
        g._pad0 = self.current_touchups().len() as f32;
        g
    }

    pub(super) fn apply_touchups(&mut self, touchups: Vec<TouchUp>) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if touchups.len() > 64 {
            self.set_status(crate::i18n::t().touch_up_limit.into());
            return;
        }
        if touchups.is_empty() {
            self.touchups.remove(&path);
        } else {
            self.touchups.insert(path.clone(), touchups.clone());
        }
        self.catalog.set_touchups(&path, &touchups);
        self.touchup_selected = None;
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    pub(super) fn choose_touchup(&self, u: f32, v: f32) -> Option<TouchUp> {
        let path = self.shown.path()?;
        // Samples by UV, so whichever loupe tier has landed will do.
        let img = self.loader.as_ref()?.get_best(path, self.preview_px())?;
        let radius = self.touchup_radius();
        let (radius_u, radius_v) = self.touchup_uv_radii(radius);
        let sample = |u: f32, v: f32| {
            let x = (u.clamp(0.0, 1.0) * (img.width.saturating_sub(1)) as f32).round() as u32;
            let y = (v.clamp(0.0, 1.0) * (img.height.saturating_sub(1)) as f32).round() as u32;
            image_ops::sample_linear(
                &img,
                x as f32 / img.width.saturating_sub(1).max(1) as f32,
                y as f32 / img.height.saturating_sub(1).max(1) as f32,
            )
        };
        // Compare colors on the patch boundary itself. A ring outside the
        // patch leaves a color step at the edge that shows as a halo.
        let ring = |cx: f32, cy: f32| -> [f32; 3] {
            let mut sum = [0.0; 3];
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let p = sample(cx + a.cos() * radius_u, cy + a.sin() * radius_v);
                for c in 0..3 {
                    sum[c] += p[c];
                }
            }
            for c in 0..3 {
                sum[c] /= 8.0;
            }
            sum
        };
        let target_ring = ring(u, v);
        let (unit_radius_u, unit_radius_v) = self.touchup_uv_radii(1.0);
        let mut best: Option<(f32, f32, f32)> = None;
        for i in 0..16 {
            let a = i as f32 * std::f32::consts::TAU / 16.0;
            let distance = radius * (3.0 + (i % 3) as f32);
            let su = u + a.cos() * distance * unit_radius_u;
            let sv = v + a.sin() * distance * unit_radius_v;
            if su < radius_u || su > 1.0 - radius_u || sv < radius_v || sv > 1.0 - radius_v {
                continue;
            }
            let sr = ring(su, sv);
            let score = (0..3)
                .map(|c| (sr[c] - target_ring[c]).powi(2))
                .sum::<f32>()
                + distance * 0.002;
            if best.map_or(true, |(s, _, _)| score < s) {
                best = Some((score, su, sv));
            }
        }
        let (_, su, sv) = best?;
        let source_ring = ring(su, sv);
        Some(TouchUp {
            center: [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)],
            radius,
            source: [su, sv],
            feather: TOUCHUP_FEATHER,
            delta: [
                target_ring[0] - source_ring[0],
                target_ring[1] - source_ring[1],
                target_ring[2] - source_ring[2],
            ],
        })
    }

    pub(super) fn add_touchup(&mut self, u: f32, v: f32) {
        let Some(t) = self.choose_touchup(u, v) else {
            self.set_status(crate::i18n::t().touch_up_needs_full.into());
            return;
        };
        let mut all = self.current_touchups().to_vec();
        all.push(t);
        let new_index = all.len() - 1;
        self.apply_touchups(all);
        self.touchup_selected = Some(new_index);
    }

    pub(super) fn delete_selected_touchup(&mut self) {
        let Some(i) = self.touchup_selected else {
            return;
        };
        let mut all = self.current_touchups().to_vec();
        if i < all.len() {
            all.remove(i);
        }
        self.apply_touchups(all);
    }

    /// True while the next Loupe click samples a pixel for white balance.
    pub(crate) fn wb_picker_active(&self) -> bool {
        self.tool == LoupeTool::WbPicker
    }

    pub(super) fn toggle_wb_picker(&mut self) {
        if self.tool == LoupeTool::WbPicker {
            self.tool = LoupeTool::None;
        } else {
            self.tool = LoupeTool::WbPicker;
            self.touchup_selected = None;
        }
        self.request_redraw();
    }

    /// Set temp/tint so the histogram-grid pixel at UV `(u, v)` becomes neutral
    /// gray. Always leaves picker mode, even when the pixel is too dark to use.
    pub(super) fn pick_white_balance(&mut self, u: f32, v: f32) {
        self.tool = LoupeTool::None;
        self.request_redraw();
        if self.hist_dw == 0 || self.hist_dh == 0 {
            self.set_status(crate::i18n::t().wb_no_image.into());
            return;
        }
        let gx = ((u * self.hist_dw as f32) as usize).min(self.hist_dw - 1);
        let gy = ((v * self.hist_dh as f32) as usize).min(self.hist_dh - 1);
        let px = self.hist_sample[gy * self.hist_dw + gx];
        match develop::neutralize_gray(px) {
            Some((temp, tint)) => {
                let mut adj = self.current_adjustments();
                adj.temp = temp;
                adj.tint = tint;
                self.apply_adjustments(adj);
            }
            None => {
                self.set_status(crate::i18n::t().wb_pick_brighter.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &App, down: bool) {
        let pos = egui::pos2(1.0, 1.0);
        let input = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: down,
                    modifiers: Default::default(),
                },
            ],
            ..Default::default()
        };
        let _ = app.egui_ctx.run_ui(input, |_| {});
    }

    fn one_photo(tag: &str) -> (App, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lp-adjust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        let mut app = App::new(None);
        app.shown = Shown::Preview(photo.clone(), 1024, 1024);
        (app, dir, photo)
    }

    #[test]
    fn a_slider_drag_writes_the_sidecar_once_on_release() {
        let (mut app, dir, photo) = one_photo("drag");
        let sidecar = dir.join(".lightphotos").join("a.jpg.xmp");
        press(&app, true);
        for step in 1..=5 {
            app.apply_adjustments(Adjustments {
                exposure: step as f32 * 0.1,
                ..Default::default()
            });
        }
        assert!(
            !sidecar.exists(),
            "nothing is written while the mouse is held"
        );

        press(&app, false);
        app.save_edit_unless_dragging();
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(sidecar.exists(), "the release writes the edit");
        assert_eq!(app.catalog.adjustments(&photo).exposure, 0.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_keyboard_nudge_is_written_at_once() {
        let (mut app, dir, _) = one_photo("key");
        app.apply_adjustments(Adjustments {
            contrast: 10.0,
            ..Default::default()
        });
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(dir.join(".lightphotos").join("a.jpg.xmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
