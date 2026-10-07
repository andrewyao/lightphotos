// SPDX-License-Identifier: MIT OR Apache-2.0

//! wasm32 decode and export of a file's bytes, which the main thread reads
//! through the File System Access API because a decode thread cannot open a
//! handle by path. The loader's threads run these jobs.

use std::path::PathBuf;
use std::sync::Arc;

use crate::image_decode::{self, DecodedImage, ImageMetadata};
use crate::thumbnail;

/// What a decode is for. `Preview` and `Full` get the full RAW demosaic with
/// linear output. `Thumb` and `Speed` (the Loupe's quick screen-fit first
/// paint) use the fast quarter-res RAW decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Thumb,
    Speed,
    Preview,
    Full,
}

impl JobKind {
    fn quality(self) -> bool {
        matches!(self, JobKind::Preview | JobKind::Full)
    }
}

/// Why a job produced no image. Chrome's `NotReadableError` can pass, so a
/// failed read is worth retrying. A decode of bytes already read fails the
/// same way every time, and each retry would read the whole file again.
#[derive(Clone, Debug)]
pub enum Failure {
    Read(String),
    Decode(String),
}

impl Failure {
    pub fn is_read(&self) -> bool {
        matches!(self, Failure::Read(_))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Read(e) | Failure::Decode(e) => f.write_str(e),
        }
    }
}

pub struct PoolResult {
    pub kind: JobKind,
    pub path: PathBuf,
    pub target: u32,
    pub result: Result<DecodedImage, Failure>,
    /// The image as a JPEG for the disk cache. Set only for thumbnails decoded
    /// from the source, and only when JPEG can represent the pixels.
    pub jpeg: Option<Vec<u8>>,
    /// The cache entry name, computed when the job was submitted, so storing
    /// the result needs no second `get_file()`.
    pub cache_name: Option<String>,
    /// The navigation generation that requested this job.
    pub generation: Option<u64>,
    /// True when the job decoded bytes from the disk cache, whether or not
    /// it succeeded.
    pub from_cache: bool,
}

impl PoolResult {
    /// A cached thumbnail that failed to decode. The caller re-reads the
    /// source without spending a retry attempt.
    pub fn needs_source_decode(&self) -> bool {
        self.kind == JobKind::Thumb && self.from_cache && self.result.is_err()
    }
}

/// One decode: the bytes and everything the result has to carry back. The
/// Loupe's jobs for one photo share one copy of its bytes.
pub struct WebJob {
    pub kind: JobKind,
    pub path: PathBuf,
    pub target: u32,
    pub bytes: Arc<Vec<u8>>,
    pub is_raw: bool,
    pub cache_name: Option<String>,
    pub generation: Option<u64>,
    pub from_cache: bool,
}

impl WebJob {
    /// What the caller sees if this job never finishes, because its thread
    /// panicked (fatal on wasm32) or it was dropped from the queue.
    pub fn failed(&self, failure: Failure) -> PoolResult {
        PoolResult {
            kind: self.kind,
            path: self.path.clone(),
            target: self.target,
            result: Err(failure),
            jpeg: None,
            cache_name: self.cache_name.clone(),
            generation: self.generation,
            from_cache: self.from_cache,
        }
    }

    pub fn run(self) -> PoolResult {
        let result = decode(&self.bytes, self.target, self.is_raw, self.kind.quality())
            .map_err(Failure::Decode);
        // A thumbnail decoded from the source is also encoded for the disk
        // cache here, off the main thread. JPEG cannot hold alpha or
        // LinearF16, so those results are not cached.
        let jpeg = match &result {
            Ok(img)
                if self.kind == JobKind::Thumb
                    && !self.from_cache
                    && thumbnail::jpeg_cacheable(img) =>
            {
                crate::image_encode::encode_jpeg_to_vec(
                    img.width,
                    img.height,
                    &img.rgba,
                    crate::image_encode::JpegQuality::Thumbnail,
                )
                .ok()
            }
            _ => None,
        };
        PoolResult {
            kind: self.kind,
            path: self.path,
            target: self.target,
            result,
            jpeg,
            cache_name: self.cache_name,
            generation: self.generation,
            from_cache: self.from_cache,
        }
    }
}

/// One metadata read. `meta` arrives holding the file facts the main thread
/// got from the `File`; the parse fills in the rest. It runs here and not on
/// the main thread because rawler builds its camera and lens tables on first
/// use behind a `Once`, and a main thread that finds a decode thread inside
/// that `Once` waits with `Atomics.wait`, which throws there. It runs on the
/// thread that runs `Preview` decodes, ahead of the photo's own `Preview`
/// and over the same bytes, so the info panel does not wait on the demosaic.
pub struct WebExifJob {
    pub path: PathBuf,
    pub bytes: Arc<Vec<u8>>,
    pub is_raw: bool,
    pub meta: ImageMetadata,
}

impl WebExifJob {
    /// The file facts alone, for a job that never finishes.
    pub fn failed(&self) -> ImageMetadata {
        ImageMetadata {
            file_size: self.meta.file_size,
            modified: self.meta.modified,
            format: self.meta.format.clone(),
            ..Default::default()
        }
    }

    pub fn run(mut self) -> ImageMetadata {
        image_decode::fill_metadata_from_bytes(&mut self.meta, &self.bytes, self.is_raw);
        self.meta
    }
}

/// One export: the source's bytes and its edits, baked into a JPEG at full
/// resolution and then fit to `max_px`. `id` finds the destination folder,
/// which is a JS object and stays on the main thread.
pub struct WebExportJob {
    pub id: u64,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub is_raw: bool,
    pub adj: crate::develop::Adjustments,
    pub touchups: Vec<crate::develop::TouchUp>,
    pub rot: u8,
    pub max_px: u32,
}

impl WebExportJob {
    pub fn run(self) -> Result<Vec<u8>, String> {
        crate::export::bake_jpeg_from_shared_vec(
            std::sync::Arc::new(self.bytes),
            self.is_raw,
            &self.adj,
            &self.touchups,
            self.rot,
            self.max_px,
        )
    }
}

/// Decode one file's bytes. RAW files try the embedded EXIF preview, then
/// rawler's embedded full image (RAF/CR3 only), then a real decode. The
/// embedded paths are faster and avoid rawler's decode path, where a panic
/// is fatal to the thread on wasm32.
///
/// `quality` picks the RAW decode: `false` is the fast quarter-res sRGB8 grid
/// decode, `true` is the full demosaic in `LinearF16`, which the renderer
/// tonemaps on the GPU. It is ignored for non-RAW files.
pub fn decode(
    bytes: &Arc<Vec<u8>>,
    max_px: u32,
    is_raw: bool,
    quality: bool,
) -> Result<DecodedImage, String> {
    if is_raw {
        if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
            // Use the same minimum resolution as native thumbnails. Tiny
            // EXIF previews must not enter the shared cache.
            if thumbnail::preview_is_large_enough(preview.width, preview.height, max_px) {
                return Ok(preview);
            }
        }
        // No size gate here. This is the camera's full-resolution JPEG, and
        // Loupe jobs ask for 8192px on WebGPU, so a gate would reject a
        // 4000px embedded JPEG and force a full demosaic.
        if let Some(preview) = thumbnail::rawler_full_image_from_bytes(bytes, max_px) {
            return Ok(preview);
        }
        return crate::raw_preview::decode_raw_from_shared_vec(Arc::clone(bytes), max_px, quality);
    }
    if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
        // Loupe jobs need more pixels than a small EXIF preview has.
        if thumbnail::preview_is_large_enough(preview.width, preview.height, max_px) {
            return Ok(preview);
        }
    }
    image_decode::decode_nonraw_from_bytes(bytes, max_px)
}
