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
        self.touchup_active
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

    /// Persist and apply `adj` to the currently-shown image: update the in-memory
    /// edits map (dropping identity edits), write the catalog, push to the GPU
    /// uniform, and mark the histogram dirty. Shared by the Develop sliders
    /// (`SetAdjustments`) and the keyboard slider nudges (`develop_adjust`).
    pub(super) fn apply_adjustments(&mut self, adj: Adjustments) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if adj.is_identity() {
            self.edits.remove(&path);
        } else {
            self.edits.insert(path.clone(), adj);
        }
        self.catalog.set_adjustments(&path, &adj);
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    /// Push the current image's adjustments into the renderer uniform. Mirrors
    /// `push_transform`; call it whenever the shown image or its edits change.
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

    /// Convert `adj` to its GPU uniform mirror, filling in `texel_w`/`texel_h`
    /// from the shown image's pixel dimensions (`GpuAdjust::from` alone can't,
    /// since it only sees `Adjustments`) — the denoise shader taps need these
    /// to offset by whole texels.
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
            self.set_status("Touch Up supports up to 64 spots".into());
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
        let img = self.loader.as_ref()?.get(&path.to_path_buf())?;
        let radius = self.touchup_radius();
        let (radius_u, radius_v) = self.touchup_uv_radii(radius);
        let sample = |u: f32, v: f32| {
            let x = (u.clamp(0.0, 1.0) * (img.width.saturating_sub(1)) as f32).round() as u32;
            let y = (v.clamp(0.0, 1.0) * (img.height.saturating_sub(1)) as f32).round() as u32;
            let i = ((y * img.width + x) * 4) as usize;
            image_ops::unpremul_to_linear([
                img.rgba[i],
                img.rgba[i + 1],
                img.rgba[i + 2],
                img.rgba[i + 3],
            ])
        };
        // Match the source and target at the actual patch boundary. Using a
        // ring outside the patch leaves a color discontinuity at the edge,
        // which becomes visible as a halo after feathering.
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
            self.set_status("Touch Up needs a full-resolution image".into());
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

    // ---- White Balance picker ----

    /// True while the next Loupe click samples a pixel for white balance.
    pub(crate) fn wb_picker_active(&self) -> bool {
        self.wb_picker
    }

    /// Toggle the WB picker on/off. Clicking the "Pick Gray" button again
    /// while it's armed cancels it without sampling anything.
    pub(super) fn toggle_wb_picker(&mut self) {
        self.wb_picker = !self.wb_picker;
        if self.wb_picker {
            self.touchup_active = false;
            self.touchup_selected = None;
        }
        self.request_redraw();
    }

    /// Sample the shown image's histogram grid at texture UV `(u, v)` and, if
    /// the pixel isn't too dark to solve reliably, set temp/tint so it
    /// becomes neutral gray. Always exits picker mode, even on a failed pick,
    /// so a stray click can't strand the user in picker mode.
    pub(super) fn pick_white_balance(&mut self, u: f32, v: f32) {
        self.wb_picker = false;
        self.request_redraw();
        if self.hist_dw == 0 || self.hist_dh == 0 {
            self.set_status("No image loaded to pick from".into());
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
                self.set_status("Pick a brighter, less saturated pixel for white balance".into());
            }
        }
    }
}
