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
    /// Decode either sRGB RGBA8 or linear RGBA16F into the linear-light domain
    /// consumed by the Develop pipeline.
    pub(super) fn build_hist_sample(&mut self, img: &image_decode::DecodedImage) {
        // Stride so the longest side maps to ~256 samples. `downsample_linear`
        // owns the striding and the degenerate-input guard; Auto Tone calls the
        // same helper on thumbnails so the two analyses see the same pixels.
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
                let out = match self.hist_pixel_format {
                    image_decode::PixelFormat::Srgb8 => develop::apply_linear(&adj, px),
                    image_decode::PixelFormat::LinearF16 => develop::apply_raw_display(&adj, px),
                };
                for ch in 0..3 {
                    // `apply_raw_display` already returns the display-space
                    // value produced by raw_shader.wgsl. The generic path
                    // returns linear output and needs the usual approximation.
                    let v = match self.hist_pixel_format {
                        image_decode::PixelFormat::Srgb8 => out[ch].max(0.0).powf(1.0 / 2.2),
                        image_decode::PixelFormat::LinearF16 => out[ch],
                    }
                    .clamp(0.0, 1.0);
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
