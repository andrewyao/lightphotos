// SPDX-License-Identifier: MIT OR Apache-2.0
//! Photo scores: the technical quality score, the judge's penalty curves, face
//! and blink quality, subject masks, and the Apple Vision requests behind them.

pub mod detect;
pub mod facequality;
pub mod judge;
pub mod quality;
// Built on macOS only for its tests, which need the `rustface` dev-dependency.
#[cfg(any(not(target_os = "macos"), test))]
pub mod seetaface;
pub mod segmentation;
pub mod sky;
#[cfg(target_os = "macos")]
pub mod vision;

#[cfg(test)]
mod test_support {
    use crate::decode::image_encode::{encode_jpeg, JpegQuality};

    /// Writes a fixture under a per-process directory, so two `cargo test` runs
    /// on one machine can't truncate each other's file mid-Vision-request.
    pub(super) fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join(name);
        encode_jpeg(&path, w, h, rgba, JpegQuality::Export).expect("encode fixture jpeg");
        path
    }
}
