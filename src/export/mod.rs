// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background export. Each export decodes as large as its size and crop
//! need, bakes in the edits, and encodes the chosen format, which takes
//! hundreds of milliseconds. A worker pool does that off the UI thread, and
//! the frame loop drains results with `poll`. Each `ExportJob` carries
//! everything its worker needs, so workers never touch `App`.

#[cfg(not(target_arch = "wasm32"))]
pub mod immich;
pub mod metadata;

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Mutex, PoisonError};
#[cfg(not(target_arch = "wasm32"))]
use std::time::SystemTime;

use crate::decode::image_encode::{Embedded, Encoding, JpegOptions, JpegPreset, Pixels};
use crate::decode::{image_decode, image_encode};
use crate::develop::sharpen::OutputSharpening;
use crate::develop::{Adjustments, MaskSet, TouchUp};
use metadata::{Keep, SourceMeta};

/// Subfolder of the current folder that exports go to, on every platform.
pub const EXPORTS_DIR: &str = "Exports";

/// What the export form is set to. Remembered across launches in `prefs`.
#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ExportSettings {
    pub target: ExportTarget,
    pub size: ExportSize,
    /// Where an Immich upload lands. Ignored for a folder export; kept apart
    /// from `target` so settings saved before albums existed still load.
    #[serde(default)]
    pub album: AlbumChoice,
    /// Sharpening for the medium, after the resize.
    #[serde(default)]
    pub sharpen: OutputSharpening,
    #[serde(default)]
    pub format: ExportFormat,
    /// Largest JPEG to write, in KB. Ignored for the other formats.
    #[serde(default)]
    pub jpeg_limit_kb: Option<u32>,
    #[serde(default)]
    pub jpeg_preset: JpegPreset,
    #[serde(default)]
    pub naming: NameTemplate,
    /// Location and identifying camera data, both off unless chosen.
    #[serde(default)]
    pub keep: Keep,
    /// Written into every export's EXIF in place of the photo's own, when
    /// not blank.
    #[serde(default)]
    pub copyright: String,
}

impl ExportSettings {
    pub fn output(&self) -> Output {
        Output {
            max_px: self.size.max_px(),
            sharpen: self.sharpen,
            format: self.format.or_jpeg(),
            jpeg_limit_kb: self.jpeg_limit_kb,
            jpeg_preset: self.jpeg_preset,
            keep: self.keep,
            copyright: self.copyright.clone(),
        }
    }
}

/// What a baked export is shaped into after its edits: its size, then its
/// output sharpening, then its file format and what it says about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Long-edge limit; `u32::MAX` keeps full size.
    pub max_px: u32,
    pub sharpen: OutputSharpening,
    pub format: ExportFormat,
    pub jpeg_limit_kb: Option<u32>,
    pub jpeg_preset: JpegPreset,
    pub keep: Keep,
    pub copyright: String,
}

impl Output {
    /// Full size, no output sharpening, a JPEG at the default quality.
    pub const FULL: Output = Output {
        max_px: u32::MAX,
        sharpen: OutputSharpening {
            media: crate::develop::sharpen::OutputMedia::Off,
            amount: crate::develop::sharpen::OutputAmount::Standard,
        },
        format: ExportFormat::Jpeg,
        jpeg_limit_kb: None,
        jpeg_preset: JpegPreset::Progressive,
        keep: Keep::NONE,
        copyright: String::new(),
    };
}

/// The file an export writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ExportFormat {
    #[default]
    Jpeg,
    Png,
    /// 16 bits per channel from the edit math; see `image_ops::bake_edited_as`.
    Tiff16,
    /// Lossless.
    Webp,
    Avif,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] = [
        ExportFormat::Jpeg,
        ExportFormat::Png,
        ExportFormat::Tiff16,
        ExportFormat::Webp,
        ExportFormat::Avif,
    ];

    /// The formats this build can write, in menu order.
    pub fn available() -> impl Iterator<Item = ExportFormat> {
        Self::ALL.into_iter().filter(|f| f.can_encode())
    }

    pub fn can_encode(self) -> bool {
        image_encode::can_encode(self.encoding(JpegOptions::default()))
    }

    /// `self`, or JPEG when this build cannot write it, such as settings
    /// saved by a build that could.
    pub fn or_jpeg(self) -> ExportFormat {
        if self.can_encode() {
            self
        } else {
            ExportFormat::Jpeg
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Jpeg => "jpg",
            ExportFormat::Png => "png",
            ExportFormat::Tiff16 => "tif",
            ExportFormat::Webp => "webp",
            ExportFormat::Avif => "avif",
        }
    }

    /// The media type an upload labels the file with.
    pub fn mime(self) -> &'static str {
        match self {
            ExportFormat::Jpeg => "image/jpeg",
            ExportFormat::Png => "image/png",
            ExportFormat::Tiff16 => "image/tiff",
            ExportFormat::Webp => "image/webp",
            ExportFormat::Avif => "image/avif",
        }
    }

    fn encoding(self, jpeg: JpegOptions) -> Encoding {
        match self {
            ExportFormat::Jpeg => Encoding::Jpeg(jpeg),
            ExportFormat::Png => Encoding::Png,
            ExportFormat::Tiff16 => Encoding::Tiff,
            ExportFormat::Webp => Encoding::Webp,
            ExportFormat::Avif => Encoding::Avif,
        }
    }
}

/// The longest stem a name template makes, in bytes: room under the common
/// 255-byte file name limit for a `-NNNN` collision suffix and an extension.
pub const MAX_STEM_BYTES: usize = 240;

/// How an export's file is named, before the extension. `{name}` is the
/// photo's own name and `{seq}` its place in the batch, from 1; everything
/// else is kept as typed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NameTemplate(pub String);

impl Default for NameTemplate {
    fn default() -> Self {
        NameTemplate("{name}".into())
    }
}

impl NameTemplate {
    /// The stem for `src`, number `seq` of `total`. `{seq}` is padded to the
    /// width of `total`, at least 3 digits, so the files sort in order.
    /// Tokens are read from the template only, never from the photo's own
    /// name. Path separators and characters Windows refuses become `_`, a
    /// Windows device name gets a `_` after it, the stem is cut to
    /// [`MAX_STEM_BYTES`], and a name that comes out empty is `export`.
    pub fn stem(&self, src: &std::path::Path, seq: usize, total: usize) -> String {
        let width = total.to_string().len().max(3);
        let mut text = String::new();
        let mut rest = self.0.as_str();
        while let Some(at) = rest.find('{') {
            text.push_str(&rest[..at]);
            let tail = &rest[at..];
            if let Some(after) = tail.strip_prefix("{name}") {
                text.push_str(&crate::paths::export_stem(src));
                rest = after;
            } else if let Some(after) = tail.strip_prefix("{seq}") {
                text.push_str(&format!("{seq:0width$}"));
                rest = after;
            } else {
                text.push('{');
                rest = &tail[1..];
            }
        }
        text.push_str(rest);
        let clean: String = text
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                c if c.is_control() => '_',
                c => c,
            })
            .collect();
        let mut clean = clean.trim().trim_matches('.').to_string();
        if clean.len() > MAX_STEM_BYTES {
            let mut cut = MAX_STEM_BYTES;
            while !clean.is_char_boundary(cut) {
                cut -= 1;
            }
            clean.truncate(cut);
        }
        if clean.is_empty() {
            return "export".into();
        }
        const DEVICES: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        let device = clean.split('.').next().unwrap_or_default();
        if DEVICES.iter().any(|d| d.eq_ignore_ascii_case(device)) {
            clean.push('_');
        }
        clean
    }
}

/// The Immich album an upload goes into.
#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub enum AlbumChoice {
    #[default]
    None,
    Existing {
        id: String,
        name: String,
    },
    /// Created when the batch finishes, then remembered as `Existing`.
    New(String),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExportTarget {
    Folder(FolderChoice),
    /// The server `App` is connected to. The browser build offers it only to
    /// say it can't: Immich sends no CORS headers, so a page can't reach one.
    Immich,
}

impl Default for ExportTarget {
    fn default() -> Self {
        ExportTarget::Folder(FolderChoice::ExportsSubfolder)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FolderChoice {
    /// `<current folder>/Exports`, resolved per batch, so each folder's
    /// exports land beside it.
    ExportsSubfolder,
    #[cfg(not(target_arch = "wasm32"))]
    Custom(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ExportSize {
    #[default]
    Full,
    /// Longest side of the finished (cropped, rotated) image, in pixels.
    LongEdge(u32),
}

impl ExportSize {
    pub const CHOICES: [ExportSize; 4] = [
        ExportSize::Full,
        ExportSize::LongEdge(4096),
        ExportSize::LongEdge(2048),
        ExportSize::LongEdge(1024),
    ];

    pub fn max_px(self) -> u32 {
        match self {
            ExportSize::Full => u32::MAX,
            ExportSize::LongEdge(px) => px,
        }
    }
}

/// Decode `src_bytes` as large as `output` needs, bake in the edits, and
/// encode the export: [`export_file`] for the wasm32 export worker, which
/// has bytes rather than a path. It already owns them in an `Arc` and passes
/// it to rawler without copying. Like the desktop exporter, it carries the
/// source's metadata as `output` says.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn bake_export_from_shared_vec(
    src_bytes: std::sync::Arc<Vec<u8>>,
    is_raw: bool,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &MaskSet,
    rot: u8,
    output: Output,
) -> Result<Vec<u8>, String> {
    let decode_px = decode_px(output.max_px, adj.crop);
    let img = if is_raw {
        crate::decode::rawler::decode_raw_nonmac_from_shared_vec(
            src_bytes.clone(),
            decode_px,
            crate::decode::rawler::RawIntent::Export,
        )?
    } else {
        image_decode::decode_nonraw_from_bytes(src_bytes.as_slice(), decode_px)?
    };
    render(
        &img,
        adj,
        touchups,
        masks,
        rot,
        &output,
        &SourceMeta::read_bytes(&src_bytes),
    )
}

/// Held while a 16-bit TIFF decodes, bakes and encodes. Its u16 bake, RGB
/// copy and file are each twice an 8-bit export's, about 600 MB at 24 MP,
/// so the desktop pool runs them one at a time. The browser holds them to
/// one in flight instead (`WebExports::capacity`).
#[cfg(not(target_arch = "wasm32"))]
static TIFF16_GATE: Mutex<()> = Mutex::new(());

/// Bake `img`'s edits, shape it per `output`, and encode it with the EXIF
/// block `output`'s policy makes of `source`.
fn render(
    img: &image_decode::DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &MaskSet,
    rot: u8,
    output: &Output,
    source: &SourceMeta,
) -> Result<Vec<u8>, String> {
    // The EXIF block and the XMP packet for a `w` x `h` export.
    let meta = |w, h| -> Result<FileMeta, String> {
        let fields = metadata::fields(source, output.keep, &output.copyright, w, h);
        let xmp = metadata::xmp(&fields);
        let block = metadata::block(&fields)?;
        // A JPEG's EXIF segment, and the carrier macOS reads AVIF's from,
        // hold 64 KB; past that the date and copyright still go in.
        let capped = matches!(output.format, ExportFormat::Jpeg | ExportFormat::Avif);
        let block = match block {
            Some(b) if capped && b.len() > image_encode::MAX_JPEG_EXIF => {
                metadata::block(&metadata::essential(&fields))?
            }
            block => block,
        };
        Ok(FileMeta { exif: block, xmp })
    };
    if output.format == ExportFormat::Tiff16 {
        let (w, h, rgba) = bake_sized::<u16>(img, adj, touchups, masks, rot, output);
        let prepared = image_encode::Prepared::new(w, h, Pixels::Rgba16(&rgba))?;
        return prepared.encode(Encoding::Tiff, meta(w, h)?.embedded());
    }
    let (w, h, rgba) = bake_sized::<u8>(img, adj, touchups, masks, rot, output);
    let prepared = image_encode::Prepared::new(w, h, Pixels::Rgba8(&rgba))?;
    let meta = meta(w, h)?;
    if output.format != ExportFormat::Jpeg {
        return prepared.encode(
            output.format.encoding(JpegOptions::default()),
            meta.embedded(),
        );
    }
    let preset = output.jpeg_preset;
    let jpeg = |quality| {
        prepared.encode(
            Encoding::Jpeg(JpegOptions { quality, preset }),
            meta.embedded(),
        )
    };
    let Some(kb) = output.jpeg_limit_kb else {
        return jpeg(None);
    };
    // Built on the first call, so an export that fits at the default
    // quality never copies the sample.
    let sample_rgba = std::cell::OnceCell::new();
    let sample = std::cell::OnceCell::new();
    let sample_len = |quality| {
        let (rows, pixels) = sample_rgba.get_or_init(|| sample_rows(w, &rgba)).as_ref()?;
        let prepared = sample
            .get_or_init(|| image_encode::Prepared::new(w, *rows, Pixels::Rgba8(pixels)).ok())
            .as_ref()?;
        let encoding = Encoding::Jpeg(JpegOptions { quality, preset });
        prepared
            .encode(encoding, Embedded::NONE)
            .ok()
            .map(|bytes| bytes.len())
    };
    within_limit(kb, meta.len(), jpeg, sample_len)
}

/// Rows in one band of [`sample_rows`]: two 4:2:0 MCU rows, so each band
/// compresses as it does in the whole photo.
const SAMPLE_BAND: usize = 32;
/// [`sample_rows`] keeps one band in this many.
const SAMPLE_STRIDE: usize = 8;

/// Every [`SAMPLE_STRIDE`]th band of `SAMPLE_BAND` rows of a `w`-wide RGBA
/// image, stacked, with its row count. A JPEG's size is the sum of its
/// blocks', so the sample's size tracks the photo's across qualities at an
/// eighth of the cost to encode. `None` for a photo too short to sample.
fn sample_rows(w: u32, rgba: &[u8]) -> Option<(u32, Vec<u8>)> {
    let row = (w as usize).checked_mul(4).filter(|&r| r > 0)?;
    let band = row.checked_mul(SAMPLE_BAND)?;
    let bands: Vec<&[u8]> = rgba.chunks_exact(band).step_by(SAMPLE_STRIDE).collect();
    if bands.len() < 4 {
        return None;
    }
    let rows = u32::try_from(bands.len() * SAMPLE_BAND).ok()?;
    Some((rows, bands.concat()))
}

/// An export's EXIF block and XMP packet, serialized.
struct FileMeta {
    exif: Option<Vec<u8>>,
    xmp: Option<Vec<u8>>,
}

impl FileMeta {
    /// Bytes the metadata adds to a file, whatever its pixels.
    fn len(&self) -> usize {
        self.exif.as_ref().map_or(0, Vec::len) + self.xmp.as_ref().map_or(0, Vec::len)
    }

    fn embedded(&self) -> Embedded<'_> {
        Embedded {
            exif: self.exif.as_deref(),
            xmp: self.xmp.as_deref(),
        }
    }
}

/// The highest-quality JPEG `encode` makes at or under `kb`, trying the
/// default quality first. A JPEG only shrinks as its quality drops, so the
/// answer is the quality that fits while the one above it does not.
///
/// A full-size encode takes hundreds of milliseconds, so `sample`, the byte
/// count of a cheaper stand-in at a quality, picks the first quality to try.
/// The tries after it step away from the guess, twice as far each time,
/// until the answer is bracketed; the rest, and every try once `sample`
/// gives `None`, is a bisection. The sample carries no metadata, so the
/// `meta_len` bytes every encode has are left out of the guess.
fn within_limit(
    kb: u32,
    meta_len: usize,
    encode: impl Fn(Option<u8>) -> Result<Vec<u8>, String>,
    sample: impl Fn(Option<u8>) -> Option<usize>,
) -> Result<Vec<u8>, String> {
    let limit = u64::from(kb) * 1024;
    let fits = |len: usize| len as u64 <= limit;
    let default = encode(None)?;
    if fits(default.len()) {
        return Ok(default);
    }
    // The next quality to try off the guess, and how far past it the one
    // after goes, doubling until the answer is bracketed.
    let mut probe = predict_quality(
        default.len().saturating_sub(meta_len),
        limit.saturating_sub(meta_len as u64),
        sample,
    )
    .map(|q| (q, 1u8));
    // Qualities run 1 to 95. `fit` fits; `over` and every quality above it
    // do not.
    let (mut fit, mut over): (Option<(u8, Vec<u8>)>, u8) = (None, 96);
    loop {
        let floor = fit.as_ref().map_or(0, |(q, _)| *q);
        if over - floor <= 1 {
            break;
        }
        let q = match probe {
            Some((q, _)) if q > floor && q < over => q,
            _ => {
                probe = None;
                floor + (over - floor) / 2
            }
        };
        let bytes = encode(Some(q))?;
        let fitted = fits(bytes.len());
        if let Some((_, step)) = probe {
            let q = if fitted {
                q.saturating_add(step)
            } else {
                q.saturating_sub(step)
            };
            probe = Some((q, step.saturating_mul(2)));
        }
        if fitted {
            fit = Some((q, bytes));
        } else {
            over = q;
        }
    }
    fit.map(|(_, bytes)| bytes)
        .ok_or_else(|| format!("over the {kb} KB limit even at the lowest JPEG quality"))
}

/// The highest quality whose `sample`, scaled by the full encode's size at
/// the default quality over the sample's, fits `limit`. A bisection over
/// the sample, so it costs about seven sample encodes. `None` when the
/// sample gives none.
fn predict_quality(
    default_len: usize,
    limit: u64,
    sample: impl Fn(Option<u8>) -> Option<usize>,
) -> Option<u8> {
    let base = sample(None)?;
    if base == 0 {
        return None;
    }
    let scale = default_len as f64 / base as f64;
    let (mut lo, mut hi, mut best) = (1u8, 95u8, 1u8);
    while lo <= hi {
        let q = lo + (hi - lo) / 2;
        if sample(Some(q))? as f64 * scale <= limit as f64 {
            best = q;
            lo = q + 1;
        } else if q == 1 {
            break;
        } else {
            hi = q - 1;
        }
    }
    Some(best)
}

/// Bake the edits into the decode, shrink the result to `output.max_px` on
/// its long side, then sharpen it for its medium. The limit applies after
/// crop and rotation because that is what "2048 px" means to someone
/// exporting, so an export decodes at full size; output sharpening comes
/// last because it is tuned to output pixels.
fn bake_sized<S: crate::develop::image_ops::Sample>(
    img: &image_decode::DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &MaskSet,
    rot: u8,
    output: &Output,
) -> (u32, u32, Vec<S>) {
    let (w, h, rgba) =
        crate::develop::image_ops::bake_edited_as::<S>(img, adj, touchups, masks, rot);
    let (w, h, mut rgba) = crate::develop::image_ops::fit_long_edge(w, h, rgba, output.max_px);
    output.sharpen.apply(w, h, &mut rgba);
    (w, h, rgba)
}

/// Where one photo's export goes. The browser writes its exports from the Web
/// Worker pool instead, so it has no jobs.
#[cfg(not(target_arch = "wasm32"))]
pub enum ExportDest {
    /// Already collision-free (see `paths::export_target`), so workers
    /// never race on file names.
    Folder(PathBuf),
    Immich {
        server: Arc<crate::export::immich::ImmichServer>,
        filename: String,
        /// 0 leaves the asset unrated.
        stars: u8,
    },
}

/// One photo to export, with everything its worker needs.
#[cfg(not(target_arch = "wasm32"))]
pub struct ExportJob {
    pub src: PathBuf,
    pub dest: ExportDest,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub masks: crate::develop::MaskInput,
    pub rot: u8,
    pub output: Output,
}

/// What an export produced.
#[derive(Debug)]
pub enum ExportLanding {
    File(PathBuf),
    #[cfg(not(target_arch = "wasm32"))]
    Asset {
        id: String,
        /// The server already had these exact bytes and kept its copy.
        duplicate: bool,
        /// Why the star rating couldn't be set on an asset that did upload.
        rating_error: Option<String>,
    },
}

/// A finished export, keyed by source path so the UI can report failures.
pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<ExportLanding, String>,
}

pub struct Exporter {
    // wasm32 exports run on the Web Worker pool instead, so it has no workers.
    #[cfg(not(target_arch = "wasm32"))]
    pool: crate::worker_pool::WorkerPool<ExportJob, ExportLanding>,
}

impl Exporter {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self::with_runner(do_export)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self {}
    }

    /// `new`, with the per-photo work swapped out so a test can make it panic.
    #[cfg(not(target_arch = "wasm32"))]
    fn with_runner(run: fn(&ExportJob) -> Result<ExportLanding, String>) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // rawler panics on some malformed files. The pool still reports the
        // photo, and the worker stays alive for the rest of the batch.
        let pool = crate::worker_pool::WorkerPool::spawn(
            "export",
            cores.saturating_sub(2).max(1),
            || {},
            run,
            |job| format!("export panicked: {}", job.src.display()),
        );
        Self { pool }
    }

    /// Queue a photo for export. Dropped silently after shutdown.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn submit(&self, job: ExportJob) {
        self.pool.submit(job);
    }

    /// Drain finished exports without blocking.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll(&self) -> Vec<ExportOutcome> {
        self.pool
            .poll()
            .into_iter()
            .map(|(job, result)| ExportOutcome {
                src: job.src,
                result,
            })
            .collect()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn poll(&self) -> Vec<ExportOutcome> {
        Vec::new()
    }
}
impl Default for Exporter {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode `job.src`, bake in its edits and size, and deliver the encoded
/// file to `job.dest`.
#[cfg(not(target_arch = "wasm32"))]
#[hotpath::measure]
fn do_export(job: &ExportJob) -> Result<ExportLanding, String> {
    // Taken before the decode, so the workers waiting their turn hold no
    // full-size image either.
    let _one_at_a_time = (job.output.format == ExportFormat::Tiff16)
        .then(|| TIFF16_GATE.lock().unwrap_or_else(PoisonError::into_inner));
    let masks = job
        .masks
        .clone()
        .resolve(&job.src)
        .map_err(|e| e.to_string())?;
    let file = export_file(
        &job.src,
        &job.adj,
        &job.touchups,
        &masks,
        job.rot,
        &job.output,
    )?;
    match &job.dest {
        ExportDest::Folder(dest) => {
            crate::paths::write_atomic(dest, &file).map_err(|e| format!("write: {e}"))?;
            Ok(ExportLanding::File(dest.clone()))
        }
        ExportDest::Immich {
            server,
            filename,
            stars,
        } => {
            // Immich dates an asset by its EXIF, which now carries the
            // camera's clock. This is the fallback it uses when that is
            // missing, so it has to be a real instant: the capture time when
            // the camera recorded its offset, else when the file was written.
            let taken = SourceMeta::read_path(&job.src)
                .stamp()
                .and_then(|s| s.instant())
                .or_else(|| std::fs::metadata(&job.src).and_then(|m| m.modified()).ok())
                .unwrap_or_else(SystemTime::now);
            let asset = server.upload(&file, filename, job.output.format.mime(), taken)?;
            // The photo is on the server either way, so a rating the key
            // isn't allowed to set is a warning, not a failed export.
            let rating_error = (1..=5)
                .contains(stars)
                .then(|| server.set_rating(&asset.id, *stars).err())
                .flatten();
            Ok(ExportLanding::Asset {
                id: asset.id,
                duplicate: asset.duplicate,
                rating_error,
            })
        }
    }
}

/// Decode `src` only as large as `output.max_px` needs, bake in its edits,
/// and encode it as `output` says: the bytes an export of `src` writes. The
/// shrink happens before the bake, so time and memory follow
/// `output.max_px`, not the source.
#[cfg(not(target_arch = "wasm32"))]
pub fn export_file(
    src: &Path,
    adj: &Adjustments,
    touchups: &[TouchUp],
    masks: &MaskSet,
    rot: u8,
    output: &Output,
) -> Result<Vec<u8>, String> {
    let img = image_decode::decode(src, decode_px(output.max_px, adj.crop))?;
    render(
        &img,
        adj,
        touchups,
        masks,
        rot,
        output,
        &SourceMeta::read_path(src),
    )
}

/// The long edge to decode at so the crop still holds `max_px` pixels on
/// its long edge. The crop is a fraction of the decoded canvas, and the
/// photo's long edge is one of its sides, so a crop whose narrower fraction
/// is `f` keeps at least `f` of the decode's long edge. Straighten turns the
/// canvas without scaling it and quarter turns only swap sides, so neither
/// changes the size needed. A crop from a hostile sidecar decodes in full.
fn decode_px(max_px: u32, crop: Option<crate::develop::Crop>) -> u32 {
    let (w, h) = crop.map_or((1.0, 1.0), |c| (c.right - c.left, c.bottom - c.top));
    if !(w > 0.0 && h > 0.0) {
        return u32::MAX;
    }
    let fraction = w.min(h).min(1.0);
    // Two pixels over, for the floor in each resize on the way. The cast
    // saturates, so a sliver of a crop asks for the full size.
    ((f64::from(max_px) / f64::from(fraction)).ceil() + 2.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    use std::io::Cursor;

    /// Callers count outcomes to know a batch is done, so a photo that panics
    /// the exporter must still produce one, and the worker must live on.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_panicking_export_still_reports_and_the_worker_survives() {
        use std::time::{Duration, Instant};

        fn panics_on_bad(job: &ExportJob) -> Result<ExportLanding, String> {
            if job.src.ends_with("bad.raw") {
                panic!("malformed file");
            }
            Ok(ExportLanding::File(PathBuf::from("out.jpg")))
        }
        let job = |name: &str| ExportJob {
            src: PathBuf::from(name),
            dest: ExportDest::Folder(PathBuf::from("out.jpg")),
            adj: Default::default(),
            touchups: Vec::new(),
            masks: crate::develop::MaskInput::none(),
            rot: 0,
            output: Output::FULL,
        };

        let exporter = Exporter::with_runner(panics_on_bad);
        // More jobs than any machine has workers, so a dead worker would strand one.
        let names: Vec<String> = (0..64)
            .map(|i| {
                if i % 2 == 0 {
                    "bad.raw".into()
                } else {
                    format!("{i}.jpg")
                }
            })
            .collect();
        for n in &names {
            exporter.submit(job(n));
        }

        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while outcomes.len() < names.len() && Instant::now() < deadline {
            outcomes.extend(exporter.poll());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(outcomes.len(), names.len(), "every job reports an outcome");
        let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
        assert_eq!(failed, 32, "each panic is reported as a failure");
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-export-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The size limit applies to the cropped, rotated result, not the decode.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_folder_export_honors_the_long_edge_after_crop_and_rotation() {
        let dir = scratch_dir("long-edge");
        let src = dir.join("src.jpg");
        image_encode::encode_jpeg(
            &src,
            400,
            200,
            &vec![128u8; 400 * 200 * 4],
            image_encode::JpegQuality::Export,
        )
        .unwrap();
        let dest = dir.join("out.jpg");
        let adj = Adjustments {
            crop: Some(crate::develop::Crop {
                left: 0.0,
                top: 0.0,
                right: 0.5,
                bottom: 1.0,
            }),
            ..Adjustments::default()
        };

        let landing = do_export(&ExportJob {
            src,
            dest: ExportDest::Folder(dest.clone()),
            adj,
            touchups: Vec::new(),
            masks: crate::develop::MaskInput::none(),
            rot: 1,
            output: Output {
                max_px: 50,
                ..Output::FULL
            },
        })
        .unwrap();

        assert!(matches!(landing, ExportLanding::File(ref p) if *p == dest));
        let out = image_decode::decode(&dest, u32::MAX).unwrap();
        assert_eq!(
            (out.width, out.height),
            (50, 50),
            "200x200 crop, rotated, fit to 50"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_export_decodes_only_what_the_crop_needs() {
        use crate::develop::Crop;
        let crop = |left, top, right, bottom| {
            Some(Crop {
                left,
                top,
                right,
                bottom,
            })
        };
        assert_eq!(decode_px(256, None), 258);
        assert_eq!(decode_px(256, crop(0.0, 0.0, 0.5, 1.0)), 514);
        assert_eq!(decode_px(256, crop(0.0, 0.25, 0.75, 0.5)), 1026);
        for hostile in [
            crop(0.5, 0.0, 0.5, 1.0),
            crop(0.9, 0.0, 0.1, 1.0),
            crop(f32::NAN, 0.0, 1.0, 1.0),
            crop(0.0, 0.0, 1e-30, 1.0),
        ] {
            assert_eq!(decode_px(256, hostile), u32::MAX, "{hostile:?}");
        }
        assert_eq!(decode_px(256, crop(-1.0, -1.0, 2.0, 2.0)), 258);
    }

    /// An export of a big photo shrinks before the bake, yet a tight crop
    /// still fills `max_px`.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn an_export_of_a_cropped_photo_still_fills_max_px() {
        let dir = scratch_dir("cropped");
        let src = dir.join("src.jpg");
        let (w, h) = (3000u32, 2000u32);
        image_encode::encode_jpeg(
            &src,
            w,
            h,
            &vec![128u8; (w * h * 4) as usize],
            image_encode::JpegQuality::Export,
        )
        .unwrap();
        let adj = Adjustments {
            crop: Some(crate::develop::Crop {
                left: 0.375,
                top: 0.25,
                right: 0.5,
                bottom: 0.5,
            }),
            straighten: 3.0,
            ..Adjustments::default()
        };
        let output = Output {
            max_px: 200,
            ..Output::FULL
        };
        assert_eq!(decode_px(200, adj.crop), 1602, "about half the source");

        let jpeg = export_file(&src, &adj, &[], &MaskSet::default(), 1, &output).unwrap();
        std::fs::write(dir.join("out.jpg"), &jpeg).unwrap();
        let out = image_decode::decode(&dir.join("out.jpg"), u32::MAX).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            out.width.max(out.height),
            200,
            "{}x{}",
            out.width,
            out.height
        );
    }

    /// Settings saved before the preset existed keep the encoder they had,
    /// and off macOS the preset picks the JPEG's kind: SOF0 is baseline,
    /// SOF2 progressive.
    #[test]
    fn the_jpeg_preset_defaults_to_progressive_and_picks_the_file_kind() {
        let old: ExportSettings =
            serde_json::from_str(r#"{"target":{"Folder":"ExportsSubfolder"},"size":"Full"}"#)
                .unwrap();
        assert_eq!(old.jpeg_preset, JpegPreset::Progressive);

        #[cfg(not(target_os = "macos"))]
        for (preset, sof) in [
            (JpegPreset::Fastest, 0xC0),
            (JpegPreset::Baseline, 0xC0),
            (JpegPreset::Progressive, 0xC2),
        ] {
            let output = Output {
                jpeg_preset: preset,
                ..Output::FULL
            };
            let jpeg = render(
                &gradient(64, 48),
                &Adjustments::default(),
                &[],
                &MaskSet::default(),
                0,
                &output,
                &SourceMeta::read_bytes(&[]),
            )
            .unwrap();
            let has = |marker| jpeg.windows(2).any(|w| w == [0xFF, marker]);
            assert!(has(sof), "{preset:?} writes SOF{:X}", sof - 0xC0);
        }
    }

    /// Output sharpening runs on the resized bake, and a settings file
    /// saved before it existed loads with it off.
    #[test]
    fn output_sharpening_runs_after_the_resize_and_defaults_off() {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
        use crate::develop::sharpen::{OutputAmount, OutputMedia, OutputSharpening};
        let (w, h) = (64u32, 32u32);
        let mut rgba = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 60 } else { 190 };
                rgba.extend([v, v, v, 255]);
            }
        }
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        });
        let small = Output {
            max_px: 32,
            ..Output::FULL
        };
        let sharp = Output {
            sharpen: OutputSharpening {
                media: OutputMedia::Screen,
                amount: OutputAmount::High,
            },
            ..small.clone()
        };
        let (pw, ph, plain) = bake_sized::<u8>(
            &img,
            &Adjustments::default(),
            &[],
            &MaskSet::default(),
            0,
            &small,
        );
        let (sw, sh, sharpened) = bake_sized::<u8>(
            &img,
            &Adjustments::default(),
            &[],
            &MaskSet::default(),
            0,
            &sharp,
        );
        assert_eq!((pw, ph), (32, 16));
        assert_eq!((sw, sh), (32, 16));
        let mut after_resize = plain.clone();
        sharp.sharpen.apply(pw, ph, &mut after_resize);
        assert_ne!(after_resize, plain);
        assert_eq!(sharpened, after_resize, "sharpened at the output size");

        let old: ExportSettings =
            serde_json::from_str(r#"{"target":{"Folder":"ExportsSubfolder"},"size":"Full"}"#)
                .unwrap();
        assert_eq!(old.sharpen.media, OutputMedia::Off);
        assert_eq!(old.format, ExportFormat::Jpeg);
        assert_eq!(old.jpeg_limit_kb, None);
        assert_eq!(old.naming, NameTemplate::default());
    }

    fn gradient(w: u32, h: u32) -> image_decode::DecodedImage {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = (x * 255 / (w - 1)) as u8;
                rgba.extend([v, (y * 7 % 256) as u8, 255 - v, 255]);
            }
        }
        DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        })
    }

    /// Every format this build offers writes a file whose own signature says
    /// so, at the baked size.
    #[test]
    fn each_available_format_writes_its_own_kind_of_file() {
        let img = gradient(48, 32);
        let mut seen = Vec::new();
        for format in ExportFormat::available() {
            let output = Output {
                format,
                ..Output::FULL
            };
            let bytes = render(
                &img,
                &Adjustments::default(),
                &[],
                &MaskSet::default(),
                1,
                &output,
                &SourceMeta::default(),
            )
            .unwrap_or_else(|e| panic!("{format:?}: {e}"));
            let magic_ok = match format {
                ExportFormat::Jpeg => bytes.starts_with(&[0xFF, 0xD8]),
                ExportFormat::Png => bytes.starts_with(b"\x89PNG"),
                ExportFormat::Tiff16 => bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"),
                ExportFormat::Webp => bytes.get(8..12) == Some(b"WEBP"),
                ExportFormat::Avif => bytes.get(4..12) == Some(b"ftypavif"),
            };
            assert!(
                magic_ok,
                "{format:?} starts {:?}",
                &bytes[..12.min(bytes.len())]
            );
            seen.push(format);
        }
        assert!(seen.contains(&ExportFormat::Png) && seen.contains(&ExportFormat::Tiff16));
    }

    /// A 16-bit TIFF holds the edit math's in-between levels: a smooth ramp
    /// pushed by an exposure edit lands on 16-bit values that are not just
    /// 8-bit ones scaled up.
    #[test]
    fn a_16_bit_bake_keeps_levels_between_8_bit_steps() {
        let img = gradient(256, 4);
        let adj = Adjustments {
            exposure: 0.37,
            ..Adjustments::default()
        };
        let (w, h, deep) =
            bake_sized::<u16>(&img, &adj, &[], &MaskSet::default(), 0, &Output::FULL);
        let (_, _, shallow) =
            bake_sized::<u8>(&img, &adj, &[], &MaskSet::default(), 0, &Output::FULL);
        assert_eq!((w, h), (256, 4));
        let between = deep.iter().filter(|&&v| v % 257 != 0).count();
        assert!(
            between > deep.len() / 2,
            "{between} of {} off the 8-bit grid",
            deep.len()
        );
        for (d, s) in deep.iter().zip(&shallow) {
            assert!((f32::from(*d) / 257.0 - f32::from(*s)).abs() <= 0.5 + 1e-3);
        }
    }

    /// A size limit lowers JPEG quality until the file fits, and a limit no
    /// quality meets is an error, not an oversized file. The photo is tall
    /// enough for the sampled guess to run.
    #[test]
    fn a_jpeg_size_limit_is_met_or_refused() {
        let (w, h) = (256u32, 1024u32);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rgba = Vec::new();
        for _ in 0..w * h {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let [r, g, b, ..] = seed.to_le_bytes();
            rgba.extend([r, g, b, 255]);
        }
        assert!(sample_rows(w, &rgba).is_some(), "the guess must run");
        let img = image_decode::DecodedImage::new_tracked(
            crate::decode::image_decode::DecodedImageFields {
                width: w,
                height: h,
                rgba,
                pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
            },
        );
        let at = |kb| {
            render(
                &img,
                &Adjustments::default(),
                &[],
                &MaskSet::default(),
                0,
                &Output {
                    jpeg_limit_kb: kb,
                    ..Output::FULL
                },
                &SourceMeta::default(),
            )
        };
        let free = at(None).unwrap();
        for eighths in [4, 6] {
            let limit = (free.len() * eighths / 8 / 1024) as u32;
            let held = at(Some(limit)).unwrap();
            assert!(
                held.len() <= limit as usize * 1024,
                "{} > {limit} KB",
                held.len()
            );
            assert!(
                held.len() * 4 > limit as usize * 1024,
                "not needlessly small"
            );
        }
        assert!(at(Some(0)).is_err(), "no JPEG fits in 0 KB");
    }

    /// Whatever the sample guesses, the answer is the highest quality that
    /// fits, with the one above it over. A right guess costs three full
    /// encodes: the default, the guess and the quality above it.
    #[test]
    fn a_size_limit_finds_the_best_quality_however_wrong_the_guess() {
        // Flat, then steep, as a real encoder's sizes run. The default is
        // over every limit.
        let size = |q: Option<u8>| {
            let q = usize::from(q.unwrap_or(90));
            1000 + q * 10 + q.saturating_sub(60) * 400
        };
        for limit_kb in [2, 4, 8, 12] {
            let limit = limit_kb * 1024;
            let best = (1..=95u8).rev().find(|&q| size(Some(q)) <= limit);
            for bias in [0.5, 0.9, 1.0, 1.1, 2.0] {
                let encodes = std::cell::Cell::new(0);
                let encode = |q| {
                    encodes.set(encodes.get() + 1);
                    Ok(vec![0u8; size(q)])
                };
                let sample = |q| Some((size(q) as f64 * bias) as usize);
                let got = within_limit(limit_kb as u32, 0, encode, sample).ok();
                assert_eq!(
                    got.map(|b| b.len()),
                    best.map(|q| size(Some(q))),
                    "{limit_kb} KB, guess off by {bias}"
                );
                if bias == 1.0 && best.is_some() {
                    assert_eq!(encodes.get(), 3, "{limit_kb} KB");
                }
            }
        }
    }

    /// The guess only saves encodes. A sample that cannot be made, or fails
    /// partway, still gives the best quality, and an export that fits at the
    /// default quality never asks for one.
    #[test]
    fn a_size_limit_needs_no_sample() {
        let size = |q: Option<u8>| 1000 + usize::from(q.unwrap_or(90)) * 100;
        let encode = |q| Ok(vec![0u8; size(q)]);
        let best = (1..=95u8).rev().find(|&q| size(Some(q)) <= 5 * 1024);
        let calls = std::cell::Cell::new(0);
        let fails_partway = |q| {
            calls.set(calls.get() + 1);
            (calls.get() < 3).then(|| size(q) / 8)
        };
        for got in [
            within_limit(5, 0, encode, |_| None),
            within_limit(5, 0, encode, fails_partway),
        ] {
            assert_eq!(got.map(|b| b.len()).ok(), best.map(|q| size(Some(q))));
        }

        let asked = std::cell::Cell::new(false);
        let sample = |q| {
            asked.set(true);
            Some(size(q))
        };
        assert!(within_limit(1000, 0, encode, sample).is_ok());
        assert!(!asked.get(), "the default fits, so no sample is needed");
    }

    /// Metadata is the same size at every quality and the sample has none,
    /// so it must not scale the guess. With 40 KB of EXIF, a right guess
    /// still costs three full encodes.
    #[test]
    fn metadata_does_not_skew_the_guess() {
        let meta = 40 * 1024;
        let pixels = |q: Option<u8>| {
            let q = usize::from(q.unwrap_or(90));
            2000 + q * 40 + q.saturating_sub(60) * 1600
        };
        let encodes = std::cell::Cell::new(0);
        let encode = |q| {
            encodes.set(encodes.get() + 1);
            Ok(vec![0u8; meta + pixels(q)])
        };
        let limit_kb = 45;
        let best = (1..=95u8)
            .rev()
            .find(|&q| meta + pixels(Some(q)) <= limit_kb * 1024);
        let got = within_limit(limit_kb as u32, meta, encode, |q| Some(pixels(q) / 8));

        assert_eq!(
            got.ok().map(|b| b.len()),
            best.map(|q| meta + pixels(Some(q)))
        );
        assert_eq!(encodes.get(), 3);
    }

    /// Every format this build writes carries the date, the camera and the
    /// copyright line, and location and serial numbers only when kept, read
    /// back by a reader that is not ours.
    #[test]
    fn every_format_carries_the_metadata_its_toggles_keep() {
        use exif::{Field, In, Tag, Value};
        let f = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
        let camera = [
            f(Tag::Make, ascii("TestCam")),
            f(Tag::DateTimeOriginal, ascii("2024:06:01 18:04:05")),
            f(Tag::GPSLatitudeRef, ascii("N")),
            f(Tag::BodySerialNumber, ascii("123456")),
            f(Tag::Copyright, ascii("Source")),
        ];
        let blob = metadata::block(&camera).unwrap().unwrap();
        let source =
            || SourceMeta::from_parts(exif::Reader::new().read_raw(blob.clone()).ok(), None);
        let img = gradient(40, 30);
        for format in ExportFormat::available() {
            for (location, identity) in [(false, false), (true, false), (false, true), (true, true)]
            {
                let output = Output {
                    format,
                    keep: Keep { location, identity },
                    copyright: "© 2026 张三 Ünïcode".into(),
                    ..Output::FULL
                };
                let what = format!("{format:?} location {location} identity {identity}");
                let bytes = render(
                    &img,
                    &Adjustments::default(),
                    &[],
                    &MaskSet::default(),
                    0,
                    &output,
                    &source(),
                )
                .unwrap_or_else(|e| panic!("{what}: {e}"));
                // The exif crate reads no AVIF; macOS's ImageIO checks those.
                if format == ExportFormat::Avif {
                    assert!(bytes.windows(4).any(|w| w == b"Exif"), "{what}");
                    continue;
                }
                let read = exif::Reader::new()
                    .read_from_container(&mut std::io::Cursor::new(&bytes))
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                let has = |tag| read.get_field(tag, In::PRIMARY).is_some();
                assert_eq!(has(Tag::GPSLatitudeRef), location, "{what}");
                assert_eq!(has(Tag::BodySerialNumber), identity, "{what}");
                assert!(has(Tag::Make) && has(Tag::DateTimeOriginal), "{what}");
                let copyright = read
                    .get_field(Tag::Copyright, In::PRIMARY)
                    .and_then(|c| match &c.value {
                        Value::Ascii(v) => v.first().cloned(),
                        _ => None,
                    });
                assert_eq!(
                    copyright.as_deref(),
                    Some("© 2026 张三 Ünïcode".as_bytes()),
                    "{what}"
                );
                let rights = "<dc:rights><rdf:Alt><rdf:li xml:lang=\"x-default\">© 2026 张三 Ünïcode</rdf:li>";
                assert!(
                    bytes.windows(rights.len()).any(|w| w == rights.as_bytes()),
                    "{what}: the XMP packet carries the copyright as UTF-8"
                );
            }
        }
    }

    /// A 16-bit TIFF export waits, before it decodes anything, while
    /// another holds the gate.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn sixteen_bit_tiffs_export_one_at_a_time() {
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let dir = scratch_dir("tiff-gate");
        let src = dir.join("src.jpg");
        image_encode::encode_jpeg(
            &src,
            16,
            8,
            &[128u8; 16 * 8 * 4],
            image_encode::JpegQuality::Export,
        )
        .unwrap();
        let job = ExportJob {
            src,
            dest: ExportDest::Folder(dir.join("out.tif")),
            adj: Default::default(),
            touchups: Vec::new(),
            masks: crate::develop::MaskInput::none(),
            rot: 0,
            output: Output {
                format: ExportFormat::Tiff16,
                ..Output::FULL
            },
        };
        let held = TIFF16_GATE.lock().unwrap();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            done_tx.send(do_export(&job).is_ok()).unwrap();
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "a second TIFF waits for the first"
        );
        drop(held);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(10)), Ok(true));
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A source whose EXIF would overflow a JPEG's 64 KB segment still
    /// exports with its date and the copyright line.
    #[test]
    fn an_oversized_exif_block_keeps_the_date_and_copyright_in_a_jpeg() {
        use exif::{Field, In, Tag, Value};
        let f = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let camera = [
            f(Tag::Make, Value::Ascii(vec![b"TestCam".to_vec()])),
            f(
                Tag::DateTimeOriginal,
                Value::Ascii(vec![b"2024:06:01 18:04:05".to_vec()]),
            ),
            f(Tag::UserComment, Value::Undefined(vec![b'x'; 70_000], 0)),
        ];
        let blob = metadata::block(&camera).unwrap().unwrap();
        let source = SourceMeta::from_parts(exif::Reader::new().read_raw(blob).ok(), None);
        let output = Output {
            keep: Keep {
                location: true,
                identity: true,
            },
            copyright: "Ada".into(),
            ..Output::FULL
        };
        let jpeg = render(
            &gradient(8, 8),
            &Adjustments::default(),
            &[],
            &MaskSet::default(),
            0,
            &output,
            &source,
        )
        .unwrap();
        let back = exif::Reader::new()
            .read_from_container(&mut std::io::Cursor::new(&jpeg))
            .unwrap();
        assert!(back.get_field(Tag::DateTimeOriginal, In::PRIMARY).is_some());
        assert!(back.get_field(Tag::Copyright, In::PRIMARY).is_some());
        assert!(back.get_field(Tag::UserComment, In::PRIMARY).is_none());
    }

    #[test]
    fn a_name_template_fills_its_tokens_and_stays_a_file_name() {
        let src = std::path::Path::new("/p/DSC_0042.ARW");
        let named = |t: &str, seq, total| NameTemplate(t.into()).stem(src, seq, total);
        assert_eq!(named("{name}", 1, 1), "DSC_0042");
        assert_eq!(named("trip-{seq}", 7, 12), "trip-007");
        assert_eq!(named("{seq}_{name}", 7, 1200), "0007_DSC_0042");
        assert_eq!(named("a/b:c", 1, 1), "a_b_c");
        assert_eq!(named("  ", 1, 1), "export");
        assert_eq!(named("..", 1, 1), "export");
        assert_eq!(NameTemplate::default().stem(src, 1, 1), "DSC_0042");
        let braced = std::path::Path::new("/p/shot_{seq}.jpg");
        assert_eq!(NameTemplate::default().stem(braced, 9, 9), "shot_{seq}");
        assert_eq!(named("{nope}-{seq}", 2, 2), "{nope}-002");
        assert_eq!(named("nul", 1, 1), "nul_");
        assert_eq!(named("Com1.x", 1, 1), "Com1.x_");
        let long = named(&"é".repeat(200), 1, 1);
        assert!(long.len() <= MAX_STEM_BYTES && long.chars().all(|c| c == 'é'));
    }

    /// With no edits, a solid-color source keeps its size and roughly its
    /// color.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn bake_export_from_shared_vec_nonraw_round_trips_identity_edit() {
        let (w, h) = (10u32, 8u32);
        let src = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 40, 40, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode source png");

        let jpeg = bake_export_from_shared_vec(
            std::sync::Arc::new(png),
            false,
            &Adjustments::default(),
            &[],
            &MaskSet::default(),
            0,
            Output::FULL,
        )
        .expect("bake_export should succeed");

        let out = image::load_from_memory(&jpeg)
            .expect("decode baked jpeg")
            .into_rgba8();
        assert_eq!(out.dimensions(), (w, h));
        let px = out.get_pixel(0, 0).0;
        assert!(px[0] > 140, "red channel should stay high, got {}", px[0]);
        assert!(
            px[1] < 100 && px[2] < 100,
            "g/b should stay low, got {},{}",
            px[1],
            px[2]
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn bake_export_from_shared_vec_applies_rotation() {
        let (w, h) = (12u32, 6u32);
        let src = image::RgbaImage::from_pixel(w, h, image::Rgba([120, 120, 120, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode source png");

        let jpeg = bake_export_from_shared_vec(
            std::sync::Arc::new(png),
            false,
            &Adjustments::default(),
            &[],
            &MaskSet::default(),
            1,
            Output::FULL,
        )
        .expect("bake_export should succeed");
        let out = image::load_from_memory(&jpeg).expect("decode").into_rgba8();
        assert_eq!(out.dimensions(), (h, w), "90 deg rotation swaps dimensions");
    }
}
