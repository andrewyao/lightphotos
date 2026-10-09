use super::*;

use crate::decode::image_decode;
use crate::decode::image_decode::PixelFormat;
use crate::develop::image_ops;
use crate::develop::{self};

pub(super) struct Histogram {
    /// Small row-major grid (`dw` x `dh`) of the shown image in
    /// linear-light RGB, so the bins recomputes cheaply as edits change.
    /// A real 2D grid so denoise can read neighbors and crop can drop cells.
    sample: Vec<[f32; 3]>,
    /// Format of the image behind `sample`. RAW linear samples need the
    /// RAW shader's sRGB transfer and preview boost.
    pixel_format: PixelFormat,
    dw: usize,
    dh: usize,
    /// Per-channel display-space bins. Float bins: each sample splits
    /// across neighboring bins, so a tone stretch doesn't leave a comb of gaps.
    bins: Option<[[f32; 256]; 3]>,
    dirty: bool,
}

impl Histogram {
    pub(super) fn new() -> Self {
        Self {
            sample: Vec::new(),
            pixel_format: PixelFormat::Srgb8,
            dw: 0,
            dh: 0,
            bins: None,
            dirty: false,
        }
    }

    /// The bins no longer match the edits, so the next frame rebins.
    pub(super) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The shown photo's sample and its format. `None` before one is shown.
    pub(super) fn sample(&self) -> Option<(&[[f32; 3]], PixelFormat)> {
        (!self.sample.is_empty()).then_some((&self.sample, self.pixel_format))
    }

    /// The sample cell under `(u, v)` in the photo's UV space.
    pub(super) fn pixel_at(&self, u: f32, v: f32) -> Option<[f32; 3]> {
        if self.dw == 0 || self.dh == 0 {
            return None;
        }
        let gx = ((u * self.dw as f32) as usize).min(self.dw - 1);
        let gy = ((v * self.dh as f32) as usize).min(self.dh - 1);
        Some(self.sample[gy * self.dw + gx])
    }
}

#[cfg(test)]
impl Histogram {
    pub(super) fn set_sample(&mut self, sample: Vec<[f32; 3]>) {
        self.sample = sample;
    }
}

impl App {
    /// Store a ~256 px linear-light downsample of a newly shown image. It stays
    /// a 2D grid (row-major, `hist_dw` × `hist_dh`) so denoise can read
    /// neighbor cells, and re-binning after an edit stays cheap.
    pub(super) fn build_hist_sample(&mut self, img: &image_decode::DecodedImage) {
        // Auto Tone uses the same helper, so both analyze the same pixels.
        const TARGET: usize = 256;
        let (sample, dw, dh) = image_ops::downsample_linear(img, TARGET);
        if sample.is_empty() {
            self.hist.sample.clear();
            self.hist.dw = 0;
            self.hist.dh = 0;
            self.hist.pixel_format = image_decode::PixelFormat::Srgb8;
            self.hist.dirty = true;
            return;
        }
        self.hist.sample = sample;
        self.hist.dw = dw;
        self.hist.dh = dh;
        self.hist.pixel_format = img.pixel_format;
        self.hist.dirty = true;
    }

    /// Rebin `hist_sample` under the current adjustments. Each cell goes through
    /// the same denoise and tone math as the preview, is encoded to display
    /// space, and lands in one of 256 buckets per channel.
    pub(super) fn recompute_histogram(&mut self) {
        if self.hist.sample.is_empty() {
            self.hist.bins = None;
            self.hist.dirty = false;
            return;
        }
        let adj = self.current_adjustments();
        // Count only cells inside the crop, matching what the loupe and export show.
        let crop = adj
            .crop
            .filter(|c| c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0);
        let (dw, dh) = (self.hist.dw, self.hist.dh);
        let grid = &self.hist.sample;
        let turn = (adj.straighten != 0.0)
            .then(|| develop::Straighten::new(adj.straighten, dw as f32, dh as f32));
        let dev = develop::Develop::new(&adj);
        let mut bins = [[0f32; 256]; 3];
        for cy in 0..dh {
            for cx in 0..dw {
                let u = (cx as f32 + 0.5) / dw as f32;
                let v = (cy as f32 + 0.5) / dh as f32;
                if let Some(c) = crop {
                    if u < c.left || u >= c.right || v < c.top || v >= c.bottom {
                        continue;
                    }
                }
                // A straightened canvas cell counts the source cell under it.
                let (gx, gy) = match turn {
                    Some(t) => {
                        let (su, sv) = t.to_source(u, v);
                        if !(0.0..1.0).contains(&su) || !(0.0..1.0).contains(&sv) {
                            continue;
                        }
                        ((su * dw as f32) as usize, (sv * dh as f32) as usize)
                    }
                    None => (cx, cy),
                };
                // Clamp neighbors to the grid, as the shader clamps to the texture.
                let px = develop::denoise_sample(&adj, |dx, dy| {
                    let sx = (gx as i64 + dx as i64).clamp(0, dw as i64 - 1) as usize;
                    let sy = (gy as i64 + dy as i64).clamp(0, dh as i64 - 1) as usize;
                    grid[sy * dw + sx]
                });
                let out = match self.hist.pixel_format {
                    image_decode::PixelFormat::Srgb8 => dev.linear(px),
                    image_decode::PixelFormat::LinearF16 => dev.raw_display(px),
                };
                for ch in 0..3 {
                    // `apply_raw_display` already returns display space. The
                    // sRGB path returns linear, so approximate with gamma 2.2.
                    let v = match self.hist.pixel_format {
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
        self.hist.bins = Some(bins);
        self.hist.dirty = false;
    }

    /// The cached histogram bins for the panel. `None` when no image is
    /// shown, and unless exactly one photo is selected, since otherwise no
    /// one photo's tones apply.
    pub(crate) fn histogram(&self) -> Option<&[[f32; 256]; 3]> {
        if self.action_count() != 1 {
            return None;
        }
        self.hist.bins.as_ref()
    }

    /// The current image's camera and exposure metadata. `None` until the
    /// background read finishes, or when nothing is selected.
    pub(crate) fn current_metadata(&self) -> Option<&image_decode::ImageMetadata> {
        self.exif_cache.get(&self.selected_path()?)
    }
}
