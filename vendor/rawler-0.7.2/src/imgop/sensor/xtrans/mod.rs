// SPDX-License-Identifier: LGPL-2.1
// Copyright 2021 Daniel Vogelbacher <daniel@chaospixel.com>

// This file is reserved for X-Trans specific sensor code.
pub mod xtrans_fast;

use multiversion::multiversion;
use rayon::prelude::*;

use crate::{
  cfa::CFA,
  imgop::{Dim2, Rect},
  pixarray::RgbF32,
};

/// Expand the monochrome X-Trans mosaic into RGB samples for the requested ROI.
/// Missing channels remain NaN for `xtrans_fast`'s neighbor interpolation pass.
#[multiversion(targets("x86_64+avx+avx2", "x86+sse", "aarch64+neon"))]
pub(crate) fn expand_xtrans_rgb(raw: &[f32], dim: Dim2, cfa: &CFA, roi: Rect) -> RgbF32 {
  let cfa_roi = cfa.shift(roi.x(), roi.y());
  let mut out = RgbF32::new_with(
    vec![[f32::NAN; 3]; roi.width() * roi.height()],
    roi.width(),
    roi.height(),
  );
  out.pixels_mut().par_chunks_exact_mut(roi.width()).enumerate().for_each(|(row_out, buf)| {
    let row_in = roi.y() + row_out;
    let line = &raw[row_in * dim.w + roi.x()..row_in * dim.w + roi.x() + roi.width()];
    for (col, (pixel, sample)) in buf.iter_mut().zip(line.iter()).enumerate() {
      pixel[cfa_roi.color_at(row_out, col)] = *sample;
    }
  });
  out
}

#[cfg(test)]
mod tests {
  use super::expand_xtrans_rgb;
  use crate::{cfa::PlaneColor, imgop::{Dim2, Point, Rect}, pixarray::PixF32};
  use crate::imgop::sensor::bayer::Demosaic;
  use super::xtrans_fast::XtransFastDemosaic;

  #[test]
  fn xtrans_demosaic_populates_all_rgb_channels() {
    let pattern = ["GRGGRG", "GGBGGB", "BGGBGG", "GRGGRG", "GGBGGB", "BGGBGG"].concat();
    let cfa = crate::CFA::new(&pattern);
    let colors = PlaneColor::new(&cfa.name);
    let raw: Vec<f32> = (0..144).map(|sample| sample as f32 + 1.0).collect();
    let pixels = PixF32::new_with(raw, 12, 12);
    let roi = Rect::new(Point::new(0, 0), Dim2::new(12, 12));

    let expanded = expand_xtrans_rgb(pixels.pixels(), pixels.dim(), &cfa, roi);
    assert!(expanded.pixels().iter().any(|pixel| pixel.iter().any(|value| value.is_nan())));

    let output = XtransFastDemosaic::new().demosaic(&pixels, &cfa, &colors, roi);
    assert!(output.pixels().iter().all(|pixel| pixel.iter().all(|value| value.is_finite())));
    assert!(output.pixels().iter().all(|pixel| pixel.iter().any(|&value| value > 0.0)));
  }
}
