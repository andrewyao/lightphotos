use super::*;

use crate::develop::{self};
use crate::{image_decode, image_ops};

impl App {
    /// Store a ~256 px linear-light downsample of a newly shown image. It stays
    /// a 2D grid (row-major, `hist_dw` × `hist_dh`) so denoise can read
    /// neighbor cells, and re-binning after an edit stays cheap.
    pub(super) fn build_hist_sample(&mut self, img: &image_decode::DecodedImage) {
        // Auto Tone uses the same helper, so both analyze the same pixels.
        const TARGET: usize = 256;
        let (sample, dw, dh) = image_ops::downsample_linear(img, TARGET);
        if sample.is_empty() {
            self.hist_sample.clear();
            self.hist_dw = 0;
            self.hist_dh = 0;
            self.hist_pixel_format = image_decode::PixelFormat::Srgb8;
            self.hist_dirty = true;
            return;
        }
        self.hist_sample = sample;
        self.hist_dw = dw;
        self.hist_dh = dh;
        self.hist_pixel_format = img.pixel_format;
        self.hist_dirty = true;
    }

    /// Rebin `hist_sample` under the current adjustments. Each cell goes through
    /// the same denoise and tone math as the preview, is encoded to display
    /// space, and lands in one of 256 buckets per channel.
    pub(super) fn recompute_histogram(&mut self) {
        if self.hist_sample.is_empty() {
            self.histogram = None;
            self.hist_dirty = false;
            return;
        }
        let adj = self.current_adjustments();
        // Count only cells inside the crop, matching what the loupe and export show.
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
                // Clamp neighbors to the grid, as the shader clamps to the texture.
                let px = develop::denoise_sample(&adj, |dx, dy| {
                    let sx = (gx as i64 + dx as i64).clamp(0, dw as i64 - 1) as usize;
                    let sy = (gy as i64 + dy as i64).clamp(0, dh as i64 - 1) as usize;
                    grid[sy * dw + sx]
                });
                let out = match self.hist_pixel_format {
                    image_decode::PixelFormat::Srgb8 => develop::apply_linear(&adj, px),
                    image_decode::PixelFormat::LinearF16 => develop::apply_raw_display(&adj, px),
                };
                for ch in 0..3 {
                    // `apply_raw_display` already returns display space. The
                    // sRGB path returns linear, so approximate with gamma 2.2.
                    let v = match self.hist_pixel_format {
                        image_decode::PixelFormat::Srgb8 => out[ch].max(0.0).powf(1.0 / 2.2),
                        image_decode::PixelFormat::LinearF16 => out[ch],
                    }
                    .clamp(0.0, 1.0);
                    // Split each sample between its two nearest buckets. Rounding
                    // to one bucket leaves a comb of gaps after a tone stretch.
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

    /// The current image's camera and exposure metadata. `None` until the
    /// background read finishes, or when nothing is selected.
    pub(crate) fn current_metadata(&self) -> Option<&image_decode::ImageMetadata> {
        self.exif_cache.get(&self.selected_path()?)
    }
}
