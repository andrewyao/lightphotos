use super::*;


use crate::develop::{self};
use crate::{image_decode, image_ops};

impl App {

    /// Build the histogram sample from a freshly-shown image: a strided
    /// downsample (~256 px on the longest side) of LINEAR-light RGB, kept as a
    /// real 2D grid (row-major, `hist_dw` × `hist_dh`) so `recompute_histogram`
    /// can look up actual neighbor cells (needed for denoise) as well as
    /// re-bin cheaply as adjustments change.
    ///
    /// The decode is premultiplied sRGB RGBA8; we un-premultiply (guarding a==0)
    /// and convert sRGB → linear with the 2.2 gamma `apply_linear` assumes, so
    /// the histogram domain matches the develop pipeline's input.
    pub(super) fn build_hist_sample(&mut self, img: &image_decode::DecodedImage) {
        let (w, h) = (img.width as usize, img.height as usize);
        if w == 0 || h == 0 || img.rgba.len() < w * h * 4 {
            self.hist_sample.clear();
            self.hist_dw = 0;
            self.hist_dh = 0;
            self.hist_dirty = true;
            return;
        }
        // Stride so the longest side maps to ~256 samples.
        const TARGET: usize = 256;
        let step = (w.max(h) / TARGET).max(1);
        let (dw, dh) = (w.div_ceil(step), h.div_ceil(step));
        let mut sample = Vec::with_capacity(dw * dh);
        let mut y = 0;
        while y < h {
            let mut x = 0;
            while x < w {
                let i = (y * w + x) * 4;
                let lin = image_ops::unpremul_to_linear([
                    img.rgba[i],
                    img.rgba[i + 1],
                    img.rgba[i + 2],
                    img.rgba[i + 3],
                ]);
                sample.push(lin);
                x += step;
            }
            y += step;
        }
        self.hist_sample = sample;
        self.hist_dw = dw;
        self.hist_dh = dh;
        self.hist_dirty = true;
    }

    /// Recompute the cached histogram from `hist_sample` under the current
    /// image's adjustments: run the same denoise formula export/preview use
    /// (on this grid's own resolution — see `denoise_sample`'s doc comment),
    /// then `apply_linear` per cell, gamma-encode the linear output to display
    /// space (matching what the shader puts on screen), and bin each channel
    /// into 256 buckets.
    pub(super) fn recompute_histogram(&mut self) {
        if self.hist_sample.is_empty() {
            self.histogram = None;
            self.hist_dirty = false;
            return;
        }
        let adj = self.current_adjustments();
        // Restrict to the active crop rect so the histogram reflects what the
        // loupe/export actually show. A full-frame (or absent) crop keeps every
        // cell. (u, v) are derived analytically from the cell's grid position.
        let crop = adj
            .crop
            .filter(|c| c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0);
        let (dw, dh) = (self.hist_dw, self.hist_dh);
        let grid = &self.hist_sample;
        let mut bins = [[0f32; 256]; 3];
        for gy in 0..dh {
            for gx in 0..dw {
                let u = (gx as f32 + 0.5) / dw as f32;
                let v = (gy as f32 + 0.5) / dh as f32;
                if let Some(c) = crop {
                    if u < c.left || u >= c.right || v < c.top || v >= c.bottom {
                        continue;
                    }
                }
                // Denoise reads neighbor cells clamped to this grid's own
                // bounds — the grid is the histogram's whole "image", the same
                // way the shader/export clamp to the full source texture.
                let px = develop::denoise_sample(&adj, |dx, dy| {
                    let sx = (gx as i64 + dx as i64).clamp(0, dw as i64 - 1) as usize;
                    let sy = (gy as i64 + dy as i64).clamp(0, dh as i64 - 1) as usize;
                    grid[sy * dw + sx]
                });
                let out = develop::apply_linear(&adj, px);
                for ch in 0..3 {
                    // Linear → display gamma (the same encoding the shader output gets).
                    let v = out[ch].max(0.0).powf(1.0 / 2.2).clamp(0.0, 1.0);
                    // Fractional ("float") binning: splat the sample across its two
                    // neighbouring buckets by sub-bin position instead of rounding to
                    // one. Spreading the energy continuously is what keeps the curve
                    // smooth after a tone stretch, rather than re-quantizing to a comb.
                    let pos = v * 255.0;
                    let lo = pos.floor();
                    let frac = pos - lo;
                    let lo = lo as usize;
                    bins[ch][lo] += 1.0 - frac;
                    if lo < 255 {
                        bins[ch][lo + 1] += frac;
                    }
                }
            }
        }
        self.histogram = Some(bins);
        self.hist_dirty = false;
    }

    /// The cached histogram bins for the panel (`None` when no image is shown).
    pub(crate) fn histogram(&self) -> Option<&[[f32; 256]; 3]> {
        self.histogram.as_ref()
    }

    /// The current image's cached camera/lens/exposure metadata, for the Loupe
    /// info panel (`None` until the background read completes, or if there's
    /// no image selected).
    pub(crate) fn current_metadata(&self) -> Option<&image_decode::ImageMetadata> {
        self.exif_cache.get(&self.selected_path()?)
    }
}
