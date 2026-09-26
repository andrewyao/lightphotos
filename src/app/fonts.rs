//! The UI font stack. Each platform adds what it can read: macOS leads with
//! its own UI font, Windows and Linux add a system Chinese font behind egui's
//! built-ins, and the browser fetches the full Noto Sans SC once a name needs
//! it. Everywhere but macOS, a small subset of Noto Sans SC covering the UI's
//! own Chinese text comes last, so the UI never draws boxes when no Chinese
//! font is installed.

use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// One face of a font file. A `.ttc` collection holds several, picked by `index`.
struct Face {
    name: &'static str,
    bytes: Vec<u8>,
    index: u32,
}

/// Install the font stack. `full_cjk` is the complete Noto Sans SC, which only
/// the web build has, once [`fetch_full_cjk`] lands it.
pub(crate) fn configure(ctx: &egui::Context, full_cjk: Option<Vec<u8>>) {
    ctx.set_fonts(build(full_cjk));
}

/// The font stack `configure` installs, split out so a test can inspect it.
fn build(full_cjk: Option<Vec<u8>>) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    let builtins = definitions.families[&FontFamily::Proportional].clone();

    let (leading, mut trailing) = system_faces();
    if let Some(bytes) = full_cjk {
        trailing.push(Face {
            name: "noto-sans-sc",
            bytes,
            index: 0,
        });
    }
    // Built by `scripts/subset-cjk-font.sh` from the CJK text in `i18n.rs`.
    // macOS always has Hiragino, so it skips the copy.
    #[cfg(not(target_os = "macos"))]
    trailing.push(Face {
        name: "noto-sans-sc-ui-subset",
        bytes: include_bytes!("../../assets/fonts/NotoSansSC-ui-subset.otf").to_vec(),
        index: 0,
    });
    let leading = install(&mut definitions, leading);
    let trailing = install(&mut definitions, trailing);

    // egui's built-in proportional faces have no arrows (U+2190-2193) or
    // U+2212, which the shortcut overlay draws. Hack has them, so it goes last
    // in the family as a glyph-of-last-resort; being last it never changes the
    // look of ordinary text.
    let last_resort: Vec<String> = definitions
        .families
        .get(&FontFamily::Monospace)
        .and_then(|family| family.first().cloned())
        .into_iter()
        .collect();
    let proportional: Vec<String> = leading
        .iter()
        .chain(&builtins)
        .chain(&trailing)
        .chain(&last_resort)
        .cloned()
        .collect();
    // Monospace keeps Hack first and uses the rest only for glyphs Hack lacks.
    // Putting a system face ahead of it would make monospace text proportional.
    if let Some(monospace) = definitions.families.get_mut(&FontFamily::Monospace) {
        monospace.extend(leading.iter().chain(&trailing).cloned());
    }
    align_baselines(&mut definitions, &proportional, &builtins);
    definitions
        .families
        .insert(FontFamily::Proportional, proportional);
    definitions
}

/// Add each face that parses to `definitions` and return the names added.
fn install(definitions: &mut FontDefinitions, faces: Vec<Face>) -> Vec<String> {
    let mut names = Vec::new();
    for face in faces {
        if face_metrics(&face.bytes, face.index, 1.0).is_none() {
            continue;
        }
        let mut data = FontData::from_owned(face.bytes);
        data.index = face.index;
        definitions
            .font_data
            .insert(face.name.to_owned(), Arc::new(data));
        names.push(face.name.to_owned());
    }
    names
}

/// Put every face in `family` on the baseline of its first face. egui already
/// tunes its built-in faces against each other, so they keep their own tweaks
/// while one of them leads the family.
fn align_baselines(definitions: &mut FontDefinitions, family: &[String], builtins: &[String]) {
    let Some(primary_name) = family.first() else {
        return;
    };
    let Some(primary) = definitions
        .font_data
        .get(primary_name)
        .and_then(|data| face_metrics(data.font.as_ref(), data.index, data.tweak.scale))
    else {
        return;
    };
    let builtin_leads = builtins.contains(primary_name);
    for name in family {
        if builtin_leads && builtins.contains(name) {
            continue;
        }
        let Some(data) = definitions.font_data.get_mut(name) else {
            continue;
        };
        let scale = data.tweak.scale;
        let Some(metrics) = face_metrics(data.font.as_ref(), data.index, scale) else {
            continue;
        };
        // Only this function holds these Arcs, so `make_mut` doesn't copy the
        // megabytes read from disk. egui's static faces are borrowed, so
        // cloning them is cheap too.
        Arc::make_mut(data).tweak.y_offset_factor = baseline_correction(primary, metrics, scale);
    }
}

/// System faces to put ahead of egui's built-ins, and faces to put after them.
#[cfg(target_os = "macos")]
fn system_faces() -> (Vec<Face>, Vec<Face>) {
    let leading = [
        ("macos-ui", "/System/Library/Fonts/SFNS.ttf"),
        // CJK coverage SFNS lacks. Index 0 of these TTCs is the regular face.
        ("macos-cjk", "/System/Library/Fonts/Hiragino Sans GB.ttc"),
        (
            "macos-cjk-fallback",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        ),
    ];
    let leading = leading
        .into_iter()
        .filter_map(|(name, path)| read_face(name, path, 0))
        .collect();
    (leading, Vec::new())
}

/// Microsoft YaHei ships with Windows since Vista, SimSun long before it.
#[cfg(target_os = "windows")]
fn system_faces() -> (Vec<Face>, Vec<Face>) {
    let fonts =
        std::path::Path::new(&std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()))
            .join("Fonts");
    let chinese = ["msyh.ttc", "simsun.ttc"]
        .into_iter()
        .find_map(|file| read_face("system-cjk", &fonts.join(file), 0).filter(draws_chinese));
    (Vec::new(), chinese.into_iter().collect())
}

/// Distributions put CJK fonts in different places, so ask fontconfig.
#[cfg(all(unix, not(target_os = "macos"), not(target_arch = "wasm32")))]
fn system_faces() -> (Vec<Face>, Vec<Face>) {
    let chinese = std::process::Command::new("fc-match")
        .args(["--format=%{file}\n%{index}", "sans-serif:lang=zh-cn"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let out = String::from_utf8(out.stdout).ok()?;
            let (file, index) = out.split_once('\n')?;
            read_face("system-cjk", file, index.trim().parse().unwrap_or(0))
        })
        // fontconfig answers with its closest match, which is a Latin face
        // when no CJK font is installed.
        .filter(draws_chinese);
    (Vec::new(), chinese.into_iter().collect())
}

/// The browser can't read system fonts; [`fetch_full_cjk`] fills in.
#[cfg(target_arch = "wasm32")]
fn system_faces() -> (Vec<Face>, Vec<Face>) {
    (Vec::new(), Vec::new())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_face(name: &'static str, path: impl AsRef<std::path::Path>, index: u32) -> Option<Face> {
    let bytes = std::fs::read(path).ok()?;
    Some(Face { name, bytes, index })
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn draws_chinese(face: &Face) -> bool {
    use skrifa::MetadataProvider as _;
    skrifa::FontRef::from_index(&face.bytes, face.index)
        .is_ok_and(|font| font.charmap().map('中').is_some())
}

/// Whether `c` is Chinese text or punctuation that Noto Sans SC covers. Same
/// ranges as `i18n::tests::is_cjk` and `scripts/subset-cjk-font.sh`.
#[cfg(target_arch = "wasm32")]
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x2E80..=0x9FFF | 0xFF00..=0xFFEF)
}

/// Where `scripts/deploy-web.sh` puts the full Noto Sans SC: beside the app's
/// wasm, as `NotoSansSC-Regular.otf`.
#[cfg(target_arch = "wasm32")]
const FULL_CJK_FILE: &str = "NotoSansSC-Regular.otf";

/// Fetch the full Noto Sans SC and reinstall the font stack with it. It is
/// 8 MB, so only a session that lists a Chinese name the UI subset lacks pays
/// for it, and the browser's HTTP cache keeps it for the next visit.
///
/// `requested` is cleared if the fetch fails, so the next listing that needs
/// the font tries again.
#[cfg(target_arch = "wasm32")]
pub(crate) fn fetch_full_cjk(
    ctx: egui::Context,
    window: Option<Arc<winit::window::Window>>,
    requested: Arc<std::sync::atomic::AtomicBool>,
) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen_futures::JsFuture;

    wasm_bindgen_futures::spawn_local(async move {
        let url = format!("{}/{FULL_CJK_FILE}", crate::web_worker_pool::asset_dir());
        let bytes = async {
            let window = web_sys::window().ok_or("no window")?;
            let response: web_sys::Response = JsFuture::from(window.fetch_with_str(&url))
                .await
                .map_err(|e| format!("{e:?}"))?
                .dyn_into()
                .map_err(|_| "not a Response")?;
            if !response.ok() {
                return Err(format!("HTTP {}", response.status()));
            }
            let buffer = JsFuture::from(response.array_buffer().map_err(|e| format!("{e:?}"))?)
                .await
                .map_err(|e| format!("{e:?}"))?;
            Ok::<_, String>(js_sys::Uint8Array::new(&buffer).to_vec())
        }
        .await;
        match bytes {
            Ok(bytes) => {
                configure(&ctx, Some(bytes));
                if let Some(window) = window {
                    window.request_redraw();
                }
            }
            Err(e) => {
                web_sys::console::warn_1(&format!("{url}: {e}").into());
                requested.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
}

/// One font face's vertical metrics, in ems, already multiplied by the
/// `FontTweak::scale` epaint draws that face at.
#[derive(Clone, Copy)]
struct FaceMetrics {
    ascent: f32,
    /// Ascent + descent + line gap.
    row_height: f32,
}

fn face_metrics(bytes: &[u8], index: u32, scale: f32) -> Option<FaceMetrics> {
    use skrifa::MetadataProvider as _;
    let font = skrifa::FontRef::from_index(bytes, index).ok()?;
    let metrics = font.metrics(
        skrifa::instance::Size::unscaled(),
        skrifa::instance::LocationRef::default(),
    );
    let upem = metrics.units_per_em as f32;
    if upem <= 0.0 {
        return None;
    }
    let ascent = metrics.ascent / upem * scale;
    // Descent is negative (it points below the baseline), hence the subtraction.
    let descent = metrics.descent / upem * scale;
    let leading = metrics.leading / upem * scale;
    Some(FaceMetrics {
        ascent,
        row_height: ascent - descent + leading,
    })
}

/// The `FontTweak::y_offset_factor` that lands `face`'s baseline on `primary`'s.
///
/// egui takes a row's metrics from the family's first face, then places each
/// glyph at `face.ascent + (primary.row_height - face.row_height) / 2`. A
/// fallback face with a different ascent-to-line-height ratio sits off the
/// baseline. Hiragino's large line gap puts CJK text about a quarter em too
/// high. epaint multiplies the factor by the face's `scale`, so we divide it out.
fn baseline_correction(primary: FaceMetrics, face: FaceMetrics, scale: f32) -> f32 {
    let drawn_baseline = face.ascent + 0.5 * (primary.row_height - face.row_height);
    (primary.ascent - drawn_baseline) / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shortcut overlay names the arrow keys with their glyphs. egui's
    /// own proportional faces have none of them, so before Hack joined the
    /// family they drew as boxes wherever no system face supplied them.
    #[test]
    fn the_proportional_family_covers_the_arrow_glyphs() {
        use skrifa::MetadataProvider as _;

        let definitions = build(None);
        let family = &definitions.families[&FontFamily::Proportional];
        for arrow in ['\u{2190}', '\u{2192}', '\u{2191}', '\u{2193}', '\u{2212}'] {
            let covered = family.iter().any(|name| {
                let Some(data) = definitions.font_data.get(name) else {
                    return false;
                };
                skrifa::FontRef::from_index(data.font.as_ref(), data.index)
                    .map(|font| font.charmap().map(arrow).is_some())
                    .unwrap_or(false)
            });
            assert!(covered, "no face in the family draws {arrow:?}");
        }
    }

    /// The primary face defines the baseline, so it is never shifted.
    #[test]
    fn the_primary_face_needs_no_baseline_correction() {
        let sf = FaceMetrics {
            ascent: 0.9668,
            row_height: 1.1777,
        };
        assert_eq!(baseline_correction(sf, sf, 1.0), 0.0);
    }

    /// Hiragino's half-em line gap draws its baseline a quarter em above San
    /// Francisco's. The correction must push it down, so it is positive.
    #[test]
    fn a_cjk_face_with_a_large_line_gap_is_pushed_back_down() {
        let sf = FaceMetrics {
            ascent: 0.9668,
            row_height: 1.1777,
        };
        let hiragino = FaceMetrics {
            ascent: 0.88,
            row_height: 1.5,
        };
        let factor = baseline_correction(sf, hiragino, 1.0);
        assert!(
            (factor - 0.2479).abs() < 1e-3,
            "expected ~0.248 em down, got {factor}"
        );
    }

    /// epaint multiplies `y_offset_factor` by the face's `scale`, so a shrunk
    /// face (egui's emoji fonts) needs a larger factor for the same shift.
    #[test]
    fn a_scaled_down_face_gets_its_scale_divided_out() {
        let primary = FaceMetrics {
            ascent: 1.0,
            row_height: 1.2,
        };
        let face = FaceMetrics {
            ascent: 0.8,
            row_height: 1.2,
        };
        assert!((baseline_correction(primary, face, 1.0) - 0.2).abs() < 1e-6);
        assert!((baseline_correction(primary, face, 0.5) - 0.4).abs() < 1e-6);
    }
}
