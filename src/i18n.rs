// SPDX-License-Identifier: GPL-3.0-or-later

//! User-facing text in every supported language. Each language is one
//! `Strings` value, so a string missing from any language is a compile error.
//! Log lines, CLI output, and OS error details stay English.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::develop::{Section, SliderId};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }

    /// Parses a stored code or a BCP 47 tag such as `zh-Hans-CN` or a POSIX
    /// locale such as `zh_CN.UTF-8`. Any Chinese variant maps to `Zh`, which
    /// is Simplified.
    fn from_tag(tag: &str) -> Option<Lang> {
        let primary = tag.split(['-', '_', '.']).next()?.to_ascii_lowercase();
        match primary.as_str() {
            "en" => Some(Lang::En),
            "zh" => Some(Lang::Zh),
            _ => None,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::Zh,
        _ => Lang::En,
    }
}

pub fn set_lang(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

/// The current language's strings.
pub fn t() -> &'static Strings {
    match lang() {
        Lang::En => &EN,
        Lang::Zh => &ZH,
    }
}

/// `prefs` key holding the saved language code.
const LANGUAGE_KEY: &str = "language";

/// Pick the startup language: the saved choice, else the OS language, else
/// English.
pub fn init() {
    let lang = saved_lang()
        .or_else(|| system_tags().iter().find_map(|t| Lang::from_tag(t)))
        .unwrap_or(Lang::En);
    set_lang(lang);
}

fn saved_lang() -> Option<Lang> {
    Lang::from_tag(crate::prefs::load(LANGUAGE_KEY)?.trim())
}

/// Switch language and remember the choice for the next launch. A storage
/// failure costs one relaunch's language, so it is logged, not surfaced.
pub fn choose(lang: Lang) {
    set_lang(lang);
    if let Err(e) = crate::prefs::save(LANGUAGE_KEY, lang.code()) {
        eprintln!("[lightphotos] could not save the language: {e}");
    }
}

/// The user's preferred languages, most preferred first. Finder-launched apps
/// get no `LANG`, so macOS asks Foundation instead.
#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
fn system_tags() -> Vec<String> {
    objc2_foundation::NSLocale::preferredLanguages()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "macos")))]
fn system_tags() -> Vec<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty())
        .collect()
}

#[cfg(target_arch = "wasm32")]
fn system_tags() -> Vec<String> {
    let Some(nav) = web_sys::window().map(|w| w.navigator()) else {
        return Vec::new();
    };
    let mut tags: Vec<String> = nav
        .languages()
        .iter()
        .filter_map(|v| v.as_string())
        .collect();
    tags.extend(nav.language());
    tags
}

/// The subject overlay is macOS-only, so elsewhere its keys do nothing and
/// the overlay leaves their section out.
const SUBJECT_KEYS: bool = crate::app::App::selection_supported();

/// One group of rows in the keyboard-shortcut overlay: (keys, description).
/// A section with no rows is not shown.
pub struct HelpSection {
    pub title: &'static str,
    pub rows: &'static [(&'static str, &'static str)],
}

/// Cmd in the native macOS app, Ctrl elsewhere. The three-argument form joins
/// two alternatives with the given word.
macro_rules! primary {
    ($keys:literal) => {
        if cfg!(all(not(target_arch = "wasm32"), target_os = "macos")) {
            concat!("Cmd", $keys)
        } else {
            concat!("Ctrl", $keys)
        }
    };
    ($a:literal, $or:literal, $b:literal) => {
        if cfg!(all(not(target_arch = "wasm32"), target_os = "macos")) {
            concat!("Cmd", $a, $or, "Cmd", $b)
        } else {
            concat!("Ctrl", $a, $or, "Ctrl", $b)
        }
    };
}

const WEB: bool = cfg!(target_arch = "wasm32");

pub struct Strings {
    // Settings dialog.
    pub settings: &'static str,
    pub settings_tip: &'static str,
    pub settings_title: &'static str,
    pub settings_theme: &'static str,
    pub settings_language: &'static str,
    pub settings_auto_tone: &'static str,
    pub autotone_center_range: &'static str,
    pub autotone_center_range_tip: &'static str,
    pub autotone_center_median: &'static str,
    pub autotone_center_median_tip: &'static str,
    pub form_general: &'static str,
    pub theme_dark: &'static str,
    pub theme_medium: &'static str,
    pub theme_light: &'static str,
    /// Each language's name, written in that language so a reader of either
    /// can find their own. The same in both catalogs.
    pub lang_english: &'static str,
    pub lang_chinese: &'static str,

    // Header and landing page.
    pub open_folder: &'static str,
    pub open_folder_tip: &'static str,
    pub back_to_grid_tip: &'static str,
    pub help_tip: &'static str,
    pub landing_prompt: &'static str,
    /// One line under the prompt saying what the app is for.
    pub landing_tagline: &'static str,
    /// The three "how it works" cards: (title, body).
    pub landing_steps: [(&'static str, &'static str); 3],
    /// Heading of the tip box that holds `landing_help`.
    pub landing_tip_title: &'static str,
    /// The tip's bullets: photos untouched with edits in sidecars, edits
    /// moving with the folder, the rename caveat, and the shortcut overlay.
    pub landing_help: [&'static str; 4],
    /// Web only: the File System Access permission the browser shows once a
    /// folder is picked. Empty off the web, where the picker is native.
    pub landing_allow_note: &'static str,
    pub opening: &'static str,
    pub choose_folder: &'static str,
    pub reopen_session: &'static str,
    /// The folder Reopen Session will open.
    pub reopen_session_tip: fn(&str) -> String,
    pub picker_title: &'static str,

    // Left panel.
    pub browse_tab: &'static str,
    pub metadata_tab: &'static str,
    pub folders_tab_tip: &'static str,
    pub info_tab_tip: &'static str,
    pub info_no_selection: &'static str,
    pub info_file: &'static str,
    pub info_camera: &'static str,
    pub info_exposure: &'static str,
    pub info_date: &'static str,
    pub info_location: &'static str,
    pub info_name: &'static str,
    pub info_size: &'static str,
    pub info_dimensions: &'static str,
    pub info_format: &'static str,
    pub info_modified: &'static str,
    pub info_make: &'static str,
    pub info_model: &'static str,
    pub info_lens: &'static str,
    pub info_shutter: &'static str,
    pub info_aperture: &'static str,
    pub info_iso: &'static str,
    pub info_focal_length: &'static str,
    pub info_exposure_comp: &'static str,
    pub info_flash: &'static str,
    pub info_white_balance: &'static str,
    pub info_captured: &'static str,
    pub info_latitude: &'static str,
    pub info_longitude: &'static str,
    pub info_altitude: &'static str,
    pub info_open_in_maps: &'static str,
    pub flash_fired: &'static str,
    pub flash_did_not_fire: &'static str,
    pub wb_auto: &'static str,
    pub wb_manual: &'static str,

    // Toolbar.
    pub rating_filter: &'static str,
    pub all: &'static str,
    pub at_least_n_stars: &'static str,
    pub exactly_n_stars: &'static str,
    pub at_most_n_stars: &'static str,
    pub show_rated: fn(&str, u8) -> String,
    pub unrated: &'static str,
    pub unrated_tip: &'static str,
    pub bursts: &'static str,
    pub bursts_needs_no_filter: &'static str,
    pub bursts_tip: &'static str,
    pub duplicates: &'static str,
    pub duplicates_tip: &'static str,
    pub eyes_closed: &'static str,
    pub eyes_closed_tip: &'static str,
    pub eyes_closed_needs_grouping: &'static str,
    pub n_photos: fn(usize) -> String,

    // Selection bar.
    pub no_selection: &'static str,
    pub n_selected: fn(usize) -> String,
    pub rate_menu: &'static str,
    pub clear_rating: &'static str,
    pub auto_tone: &'static str,
    pub auto_tone_selection_tip: &'static str,
    pub copy_settings: &'static str,
    pub copy_settings_tip: &'static str,
    pub copy_settings_needs_one: &'static str,
    pub apply_settings: &'static str,
    pub apply_settings_needs_copy: &'static str,
    pub preset_menu: &'static str,
    pub apply_preset_selection_tip: &'static str,
    pub settings_from: fn(&str) -> String,
    pub export_jpg: &'static str,
    pub export_jpg_tip: &'static str,
    pub delete: &'static str,
    pub delete_selection_tip: &'static str,

    // Modals.
    pub confirm: &'static str,
    pub cancel: &'static str,
    pub shortcuts_title: &'static str,
    pub close: &'static str,
    pub help: &'static [HelpSection],

    // Bulk confirmations.
    pub confirm_clear_rating: fn(usize) -> String,
    pub confirm_rate: fn(&str, usize) -> String,
    pub confirm_apply_settings: fn(usize) -> String,
    /// (preset name, photo count)
    pub confirm_apply_preset: fn(&str, usize) -> String,
    pub confirm_auto_tone: fn(usize) -> String,
    pub confirm_delete: fn(usize) -> String,
    /// Titles and confirm buttons of the bulk dialogs that no toolbar label fits.
    pub bulk_rate: &'static str,
    pub bulk_apply_preset: &'static str,
    pub bulk_delete: &'static str,

    // Develop panel.
    pub develop: &'static str,
    pub tab_sliders: &'static str,
    pub tab_crop: &'static str,
    pub tab_masks: &'static str,
    pub presets: &'static str,
    pub save_preset_tip: &'static str,
    pub import_lr_presets: &'static str,
    pub import_lr_presets_tip: &'static str,
    pub xmp_picker_title: &'static str,
    pub xmp_filter_name: &'static str,
    /// (comma-separated list of Lightroom field names)
    pub preset_import_note: fn(&str) -> String,
    pub preset_monochrome_note: &'static str,
    pub no_presets: &'static str,
    pub apply_preset_tip: &'static str,
    pub preset_actions_tip: &'static str,
    pub save_preset_title: &'static str,
    pub rename_preset_title: &'static str,
    pub preset_name_hint: &'static str,
    pub preset_name_label: &'static str,
    pub save: &'static str,
    pub rename: &'static str,
    /// The name the save button suggests, e.g. `Preset 3`.
    pub preset_default_name: fn(usize) -> String,
    pub confirm_delete_preset: fn(&str) -> String,
    pub reset: &'static str,
    pub auto_tone_tip: &'static str,
    pub touch_up: &'static str,
    pub brush_size: &'static str,
    pub brush_size_tip: &'static str,
    pub feather: &'static str,
    pub feather_tip: &'static str,
    pub spots: &'static str,
    pub pick_gray: &'static str,
    pub pick_gray_tip: &'static str,
    pub white_balance: &'static str,
    pub tone: &'static str,
    pub presence: &'static str,
    pub detail: &'static str,
    pub temp: &'static str,
    pub tint: &'static str,
    pub exposure: &'static str,
    pub contrast: &'static str,
    pub highlights: &'static str,
    pub shadows: &'static str,
    pub whites: &'static str,
    pub blacks: &'static str,
    pub vibrance: &'static str,
    pub saturation: &'static str,
    pub denoise: &'static str,

    // Loupe.
    pub before: &'static str,
    pub after: &'static str,
    pub invert: &'static str,
    pub invert_tip: &'static str,
    pub show_selection: &'static str,
    pub selection_pending: &'static str,
    pub no_subject: &'static str,
    pub show_selection_tip: &'static str,
    /// (year, month, day, hour, minute)
    pub capture_date: fn(i32, u32, u32, u32, u32) -> String,

    // Survey.
    pub survey_heading: fn(usize) -> String,
    pub keep_best: &'static str,
    pub keep_best_tip: &'static str,
    pub close_esc: &'static str,

    // Window titles.
    pub grid_title: fn(usize) -> String,
    pub survey_title: fn(usize) -> String,

    // Status messages.
    pub deleting: fn(usize, usize) -> String,
    pub deleted: fn(usize) -> String,
    pub deleted_partial: fn(usize, usize, &str) -> String,
    pub delete_in_progress: &'static str,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub delete_no_handle: fn(&str) -> String,
    pub copied_settings_from: fn(&str) -> String,
    pub applied_settings: fn(usize) -> String,
    pub saved_preset: fn(&str) -> String,
    /// (imported, skipped, the first skip's message). One closure rather than
    /// one string per case, so a partial import cannot lose one of its counts
    /// to a second, competing status message.
    pub lr_import_status: fn(usize, usize, &str) -> String,
    /// (file name, reason)
    pub lr_import_failed: fn(&str, &str) -> String,
    /// (preset name, photo count)
    pub applied_preset: fn(&str, usize) -> String,
    pub renamed_preset: fn(&str) -> String,
    pub deleted_preset: fn(&str) -> String,
    pub cleared_rating: fn(usize) -> String,
    pub rated: fn(usize, u8) -> String,
    pub kept_best: fn(usize) -> String,
    pub export_title: fn(usize) -> String,
    pub export_destination: &'static str,
    pub export_to_folder: &'static str,
    pub export_to_immich: &'static str,
    pub export_exports_subfolder: &'static str,
    pub export_chosen_folder: &'static str,
    pub export_choose_folder: &'static str,
    pub export_size: &'static str,
    pub export_folder: &'static str,
    pub export_output: &'static str,
    pub immich_account: &'static str,
    pub export_size_full: &'static str,
    pub export_size_long_edge: fn(u32) -> String,
    pub export_run: &'static str,
    pub export_needs_immich: &'static str,
    pub immich_server_url: &'static str,
    pub immich_url_example: &'static str,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub immich_native_only: &'static str,
    pub immich_api_key: &'static str,
    pub immich_connect: &'static str,
    pub immich_connecting: &'static str,
    pub immich_connected_as: fn(&str) -> String,
    pub immich_disconnect: &'static str,
    pub immich_key_storage: &'static str,
    pub uploading: fn(usize, usize) -> String,
    pub uploaded: fn(usize, usize) -> String,
    pub uploaded_partial: fn(usize, usize, &str) -> String,
    /// The upload summary, then how many ratings failed and why.
    pub ratings_not_set: fn(&str, usize, &str) -> String,
    pub immich_album: &'static str,
    pub album_none: &'static str,
    pub album_new: &'static str,
    pub album_name: &'static str,
    pub album_name_needed: &'static str,
    pub albums_failed: fn(&str) -> String,
    pub adding_to_album: fn(&str, &str) -> String,
    pub added_to_album: fn(&str, &str) -> String,
    pub album_failed: fn(&str, &str) -> String,
    pub export_nothing_selected: &'static str,
    pub export_in_progress: &'static str,
    pub export_catalog_loading: &'static str,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub export_no_handle: &'static str,
    pub export_no_folder: fn(&str) -> String,
    pub exporting: fn(usize, usize) -> String,
    pub exported: fn(usize) -> String,
    pub exported_partial: fn(usize, usize, &str) -> String,
    pub auto_tone_needs_load: &'static str,
    pub auto_tone_applied: &'static str,
    pub auto_tone_waits_for_catalog: &'static str,
    pub auto_tone_stopped: fn(usize, usize) -> String,
    pub auto_tone_applied_n: fn(usize) -> String,
    pub auto_tone_progress: fn(usize, usize) -> String,
    pub touch_up_limit: &'static str,
    pub touch_up_needs_full: &'static str,
    pub wb_no_image: &'static str,
    pub wb_pick_brighter: &'static str,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub open_folder_failed: fn(&str) -> String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub preview_failed: fn(&str) -> String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub folder_handle_missing: fn(&str) -> String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub list_folder_failed: fn(&str, &str) -> String,
    pub catalog_save_failed: fn(&str) -> String,
    pub presets_load_failed: fn(&str) -> String,
    pub presets_locked: &'static str,
    pub presets_save_failed: fn(&str) -> String,
}

impl Strings {
    pub fn section(&self, section: Section) -> &'static str {
        match section {
            Section::WhiteBalance => self.white_balance,
            Section::Tone => self.tone,
            Section::Presence => self.presence,
            Section::Detail => self.detail,
        }
    }

    pub fn slider(&self, id: SliderId) -> &'static str {
        match id {
            SliderId::Temp => self.temp,
            SliderId::Tint => self.tint,
            SliderId::Exposure => self.exposure,
            SliderId::Contrast => self.contrast,
            SliderId::Highlights => self.highlights,
            SliderId::Shadows => self.shadows,
            SliderId::Whites => self.whites,
            SliderId::Blacks => self.blacks,
            SliderId::Vibrance => self.vibrance,
            SliderId::Saturation => self.saturation,
            SliderId::Denoise => self.denoise,
        }
    }
}

pub static EN: Strings = Strings {
    settings: "Settings",
    settings_tip: "Theme, language and Auto Tone (Cmd+,)",
    settings_title: "Settings",
    settings_theme: "Theme",
    settings_language: "Language",
    settings_auto_tone: "Auto Tone",
    autotone_center_range: "Center the range",
    autotone_center_range_tip: "Put the middle of the histogram's range at 50%. Keeps the photo's shape where it is.",
    autotone_center_median: "Center the middle pixel",
    autotone_center_median_tip: "Put the pixel half the photo is darker than at 50%. A photo that is mostly shadow comes out brighter.",
    form_general: "General",
    theme_dark: "Dark",
    theme_medium: "Medium",
    theme_light: "Light",
    lang_english: "English",
    lang_chinese: "中文",

    open_folder: "Open\u{2026}",
    open_folder_tip: "Open a different folder (Cmd+O)",
    back_to_grid_tip: "Back to the grid (G)",
    help_tip: "Keyboard shortcuts (?)",
    browse_tab: "Browse",
    metadata_tab: "Metadata",
    folders_tab_tip: "Browse folders (I)",
    info_tab_tip: "Photo metadata (I)",
    info_no_selection: "No photo selected",
    info_file: "File",
    info_camera: "Camera",
    info_exposure: "Exposure",
    info_date: "Date",
    info_location: "Location",
    info_name: "Name",
    info_size: "Size",
    info_dimensions: "Dimensions",
    info_format: "Format",
    info_modified: "Modified",
    info_make: "Make",
    info_model: "Model",
    info_lens: "Lens",
    info_shutter: "Shutter",
    info_aperture: "Aperture",
    info_iso: "ISO",
    info_focal_length: "Focal length",
    info_exposure_comp: "Exposure comp.",
    info_flash: "Flash",
    info_white_balance: "White balance",
    info_captured: "Captured",
    info_latitude: "Latitude",
    info_longitude: "Longitude",
    info_altitude: "Altitude",
    info_open_in_maps: "Open in Maps",
    flash_fired: "Fired",
    flash_did_not_fire: "Did not fire",
    wb_auto: "Auto",
    wb_manual: "Manual",
    landing_prompt: "Choose a folder of photos to get started",
    landing_tagline: "Cull, rate and develop your raw photos, fast.",
    landing_steps: [
        (
            "Browse",
            "See every photo in the folder as a thumbnail. Press Enter to open one full size.",
        ),
        (
            "Rate",
            "Press 0-5 to give stars, then filter the library down to your keepers.",
        ),
        (
            "Develop",
            "Adjust tone and color, crop and rotate, then press X to export JPEGs.",
        ),
    ],
    landing_tip_title: "Tip",
    landing_help: [
        "Your original photos are never modified. Your edits are saved in small files in a \
         .lightphotos subfolder.",
        "When you move a folder of photos, the edits move with it.",
        "Don't rename your photos. Edits are matched to photos by file name.",
        "Press ? in the application to see all the keyboard shortcuts.",
    ],
    landing_allow_note: if WEB {
        "When the browser asks \u{201C}Allow this site to edit files? \u{2026}\u{201D}, \
         click Allow \u{2014} LightPhotos needs it to read your photos and save ratings."
    } else {
        ""
    },
    opening: "Opening\u{2026}",
    choose_folder: "Choose Folder",
    reopen_session: "Reopen Session",
    reopen_session_tip: |folder| format!("Pick up where you left off in {folder}"),
    picker_title: "Choose a folder of photos",

    rating_filter: "Rating:",
    all: "All",
    at_least_n_stars: "At least N stars",
    exactly_n_stars: "Exactly N stars",
    at_most_n_stars: "At most N stars",
    show_rated: |cmp, n| format!("Show photos rated {cmp} {n}"),
    unrated: "Unrated",
    unrated_tip: "Show only photos with no rating",
    bursts: "Bursts",
    bursts_needs_no_filter: "Clear the filter to use Bursts",
    bursts_tip: "Group bursts and badge the sharpest frame (B)",
    duplicates: "Duplicates",
    duplicates_tip: "Group visually-similar frames and badge them (D)",
    eyes_closed: "Eyes closed",
    eyes_closed_tip: "Show only photos where someone blinked",
    eyes_closed_needs_grouping: "Turn on Bursts or Duplicates to detect blinks",
    n_photos: |n| format!("{n} photos"),

    no_selection: "No selection \u{2014} click a photo, or Cmd+A to select all",
    n_selected: |n| format!("{n} selected"),
    rate_menu: "Rate \u{2605}",
    clear_rating: "Clear rating",
    auto_tone: "Auto Tone",
    auto_tone_selection_tip:
        "Set each selected photo's tone sliders from its own histogram (Cmd+Shift+U)",
    copy_settings: "Copy Settings",
    copy_settings_tip: "Copy this photo's develop settings (Cmd+Shift+C)",
    copy_settings_needs_one: "Select a single photo to copy its settings",
    apply_settings: "Apply Settings",
    apply_settings_needs_copy: "Copy settings from a photo first",
    preset_menu: "Preset",
    apply_preset_selection_tip: "Apply a saved preset to the selection",
    settings_from: |name| format!("from {name}"),
    export_jpg: "Export",
    export_jpg_tip: "Choose where and at what size, then export the selection as JPGs (X)",
    delete: "Delete",
    delete_selection_tip: if WEB {
        "Permanently delete selected photos; cannot be undone (Delete)"
    } else {
        "Move selected photos to the Trash (Delete)"
    },

    confirm: "Confirm",
    cancel: "Cancel",
    shortcuts_title: "Keyboard shortcuts",
    close: "Close",
    help: &[
        HelpSection {
            title: "Navigate",
            rows: &[
                ("Enter or Space", "Open selected photo (in library)"),
                ("Esc", "Back to library (in editor)"),
                ("\u{2190} / \u{2192}", "Previous / next photo (in editor)"),
                (
                    "\u{2190} \u{2192} \u{2191} \u{2193}",
                    "Move selection in library grid",
                ),
                ("I", "Show folders or photo info in the side panel"),
                ("?", "Show or hide this help"),
                (primary!("+,"), "Settings: theme and language"),
            ],
        },
        HelpSection {
            title: "Select",
            rows: &[
                (primary!("+A"), "Select all in current folder"),
                ("Shift+Click", "Range-select"),
                (primary!("+Click"), "Toggle individual selection"),
                ("Shift+arrows", "Extend selection (library)"),
            ],
        },
        HelpSection {
            title: "Rate",
            rows: &[("0 1 2 3 4 5", "Set star rating 0-5")],
        },
        HelpSection {
            title: "Zoom and pan (in editor)",
            rows: &[
                ("Space", "Cycle zoom: fit, 2x fit, 100%"),
                (primary!("+0", " or ", "+)"), "Fit to window"),
                (primary!("+1", " or ", "+!"), "100% (1:1 pixel)"),
                (primary!("++", " or ", "+="), "Zoom in (20% step)"),
                (primary!("+-"), "Zoom out (20% step)"),
                ("\u{2191} / \u{2193}", "Zoom in / out (10% step)"),
                ("Drag", "Pan"),
                ("Space+Drag", "Pan"),
            ],
        },
        HelpSection {
            title: "Edit",
            rows: &[
                ("[", "Rotate image -90 degrees"),
                ("]", "Rotate image +90 degrees"),
                ("C", "Crop"),
                ("Y", "Before / after compare"),
                ("X", "Open or close the export form"),
                (
                    "Delete",
                    if WEB {
                        "Permanently delete; cannot be undone"
                    } else {
                        "Move to Trash"
                    },
                ),
            ],
        },
        HelpSection {
            title: "Subject selection",
            rows: if SUBJECT_KEYS {
                &[
                    ("O", "Show or hide the subject selection"),
                    ("Shift+O", "Invert the subject selection"),
                ]
            } else {
                &[]
            },
        },
        HelpSection {
            title: "Touch Up",
            rows: &[
                ("K", "Open the Masks tab"),
                ("Q", "Touch Up on / off"),
                ("[ / ]", "Brush size"),
                ("Scroll", "Brush size"),
                ("Shift+[ / ]", "Brush feather"),
                ("Shift+Scroll", "Brush feather"),
                ("O", "Show or hide the spots"),
                ("Shift+O", "Select the next spot"),
                ("Delete", "Delete the selected spot"),
                (primary!("+Z"), "Undo the last spot"),
                ("Esc", "Leave Touch Up"),
            ],
        },
    ],

    confirm_clear_rating: |n| format!("Clear the rating on {n} photo(s)?"),
    confirm_rate: |stars, n| format!("Apply {stars} to {n} photo(s)?"),
    confirm_apply_settings: |n| format!("Apply the copied settings to {n} photo(s)?"),
    confirm_apply_preset: |name, n| format!("Apply {name} to {n} photo(s)?"),
    confirm_auto_tone: |n| format!("Auto Tone {n} photo(s)?"),
    confirm_delete: if WEB {
        |n| format!("Permanently delete {n} photo(s)? This cannot be undone.")
    } else {
        |n| format!("Move {n} photo(s) to the Trash?")
    },
    bulk_rate: "Rate",
    bulk_apply_preset: "Apply Preset",
    bulk_delete: if WEB { "Delete Photos" } else { "Move to Trash" },

    develop: "Develop",
    tab_sliders: "Sliders",
    tab_crop: "Crop",
    tab_masks: "Masks",
    presets: "Presets",
    save_preset_tip: "Save these settings as a preset",
    import_lr_presets: "Import from Lightroom",
    import_lr_presets_tip: "Import Lightroom .xmp preset files",
    xmp_picker_title: "Choose Lightroom presets",
    xmp_filter_name: "Lightroom preset",
    preset_import_note: |list| format!("Lightroom settings with no equivalent here: {list}"),
    preset_monochrome_note: "Black and white approximated as Saturation -100",
    no_presets: "No presets yet",
    apply_preset_tip: "Apply to this photo",
    preset_actions_tip: "Rename or delete",
    save_preset_title: "Save preset",
    rename_preset_title: "Rename preset",
    preset_name_hint: "Preset name",
    preset_name_label: "Name",
    save: "Save",
    rename: "Rename",
    preset_default_name: |n| format!("Preset {n}"),
    confirm_delete_preset: |name| format!("Delete the preset {name}?"),
    reset: "Reset",
    auto_tone_tip: "Set the tone sliders from this photo's own histogram",
    touch_up: "Touch Up",
    brush_size: "Size",
    brush_size_tip: "[ / ] or the scroll wheel",
    feather: "Feather",
    feather_tip: "Shift+[ / Shift+] or Shift+scroll",
    spots: "Spots:",
    pick_gray: "Pick Gray",
    pick_gray_tip: "Click a neutral-gray pixel in the image",
    white_balance: "White Balance",
    tone: "Tone",
    presence: "Presence",
    detail: "Detail",
    temp: "Temp",
    tint: "Tint",
    exposure: "Exposure",
    contrast: "Contrast",
    highlights: "Highlights",
    shadows: "Shadows",
    whites: "Whites",
    blacks: "Blacks",
    vibrance: "Vibrance",
    saturation: "Saturation",
    denoise: "Denoise",

    before: "Before",
    after: "After",
    invert: "Invert",
    invert_tip: "Highlight the background instead of the subject",
    show_selection: "Show Selection",
    selection_pending: "Selection\u{2026}",
    no_subject: "No subject",
    show_selection_tip: "Outline the subject Vision finds in this photo",
    capture_date: |year, month, day, hour, minute| {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let mon = MONTHS
            .get(month.wrapping_sub(1) as usize)
            .copied()
            .unwrap_or("");
        let (h12, ampm) = match hour {
            0 => (12, "AM"),
            1..=11 => (hour, "AM"),
            12 => (12, "PM"),
            _ => (hour - 12, "PM"),
        };
        format!("{mon} {day}, {year} {h12}:{minute:02} {ampm}")
    },

    survey_heading: |n| format!("Survey \u{2014} {n} photos"),
    keep_best: "Keep Best, Reject Rest",
    keep_best_tip:
        "Rate the best photo \u{2605}5 and every other photo in this group \u{2605}1 (Enter)",
    close_esc: "Close (Esc)",

    grid_title: |n| format!("Grid  ({n} photos)"),
    survey_title: |n| format!("Survey  ({n} photos)"),

    deleting: if WEB {
        |done, total| format!("Deleting {done}/{total}\u{2026}")
    } else {
        |done, total| format!("Moving {done}/{total} to Trash\u{2026}")
    },
    deleted: if WEB {
        |n| format!("Permanently deleted {n} photo(s)")
    } else {
        |n| format!("Moved {n} photo(s) to Trash")
    },
    deleted_partial: if WEB {
        |n, total, e| format!("Permanently deleted {n}/{total} \u{2014} last error: {e}")
    } else {
        |n, total, e| format!("Moved {n}/{total} \u{2014} last error: {e}")
    },
    delete_in_progress: "Another delete is still running\u{2026}",
    delete_no_handle: |dir| format!("Could not delete photos: no directory handle for {dir}"),
    copied_settings_from: |name| format!("Copied settings from {name}"),
    applied_settings: |n| format!("Applied settings to {n} photo(s)"),
    saved_preset: |name| format!("Saved preset {name}"),
    lr_import_status: |ok, failed, first| match (ok, failed) {
        (ok, 0) => format!("Imported {ok} preset(s)"),
        (0, _) => first.to_string(),
        (ok, failed) => format!("Imported {ok} preset(s), skipped {failed} ({first})"),
    },
    lr_import_failed: |file, reason| format!("Could not import {file}: {reason}"),
    applied_preset: |name, n| format!("Applied {name} to {n} photo(s)"),
    renamed_preset: |name| format!("Renamed preset to {name}"),
    deleted_preset: |name| format!("Deleted preset {name}"),
    cleared_rating: |n| format!("Cleared rating on {n} photo(s)"),
    rated: |n, stars| format!("Rated {n} photo(s) \u{2605}{stars}"),
    kept_best: |n| format!("Kept best, rated {n} sibling(s) \u{2605}1"),
    export_title: |n| {
        if n == 1 {
            "Export 1 photo".into()
        } else {
            format!("Export {n} photos")
        }
    },
    export_destination: "Destination",
    export_to_folder: "Folder",
    export_to_immich: "Immich",
    export_exports_subfolder: "Exports subfolder",
    export_chosen_folder: "Another folder",
    export_choose_folder: "Choose\u{2026}",
    export_size: "Size",
    export_folder: "Location",
    export_output: "Output",
    immich_account: "Account",
    export_size_full: "Full resolution",
    export_size_long_edge: |px| format!("{px} px long edge"),
    export_run: "Export",
    export_needs_immich: "Connect to an Immich server first",
    immich_server_url: "Server URL",
    immich_url_example: "Example: https://immich.gumnut.ai",
    immich_native_only: "Uploading to Immich is only available in the desktop app. Browsers can't connect to an Immich server.",
    immich_api_key: "API key",
    immich_connect: "Connect",
    immich_connecting: "Connecting\u{2026}",
    immich_connected_as: |name| format!("Connected as {name}"),
    immich_disconnect: "Disconnect",
    immich_key_storage: if cfg!(target_os = "macos") {
        "The key is kept in your Keychain."
    } else {
        "The key is kept in a file only you can read."
    },
    uploading: |done, total| format!("Uploading {done}/{total}\u{2026}"),
    uploaded: |n, dup| match dup {
        0 => format!("Uploaded {n} photo(s)"),
        _ => format!("Uploaded {n} photo(s), {dup} already on the server"),
    },
    uploaded_partial: |ok, total, e| format!("Uploaded {ok}/{total} \u{2014} last error: {e}"),
    ratings_not_set: |summary, n, e| format!("{summary} \u{2014} rating not set on {n}: {e}"),
    immich_album: "Album",
    album_none: "No album",
    album_new: "New album\u{2026}",
    album_name: "Album name",
    album_name_needed: "Name the new album first",
    albums_failed: |e| format!("Couldn't load albums: {e}"),
    adding_to_album: |summary, name| format!("{summary} \u{2014} adding to {name}\u{2026}"),
    added_to_album: |summary, name| format!("{summary} \u{2014} added to {name}"),
    album_failed: |summary, e| format!("{summary} \u{2014} not added to the album: {e}"),
    export_nothing_selected: "Export: nothing selected",
    export_in_progress: "Export already in progress\u{2026}",
    export_catalog_loading: "Export: catalog still loading, try again in a moment\u{2026}",
    export_no_handle: "Export: no directory handle for the current folder",
    export_no_folder: |e| format!("Export failed: could not create Exports folder: {e}"),
    exporting: |done, total| format!("Exporting {done}/{total}\u{2026}"),
    exported: |n| format!("Exported {n} photo(s)"),
    exported_partial: |ok, total, e| format!("Exported {ok}/{total} \u{2014} last error: {e}"),
    auto_tone_needs_load: "Auto Tone needs the photo to finish loading",
    auto_tone_applied: "Auto Tone applied",
    auto_tone_waits_for_catalog: "Auto Tone will start after the catalog loads\u{2026}",
    auto_tone_stopped: |done, total| format!("Auto Tone stopped at {done}/{total}"),
    auto_tone_applied_n: |n| format!("Auto Tone applied to {n} photos"),
    auto_tone_progress: |done, total| format!("Auto Tone {done}/{total}\u{2026}"),
    touch_up_limit: "Touch Up supports up to 64 spots",
    touch_up_needs_full: "Touch Up needs a full-resolution image",
    wb_no_image: "No image loaded to pick from",
    wb_pick_brighter: "Pick a brighter, less saturated pixel for white balance",
    open_folder_failed: |e| format!("Couldn't open folder: {e}"),
    preview_failed: |path| format!("Unable to load Loupe preview for {path}"),
    folder_handle_missing: |dir| format!("Couldn't open {dir} \u{2014} folder handle missing"),
    list_folder_failed: |dir, e| format!("Couldn't list {dir}: {e}"),
    catalog_save_failed: |e| format!("Failed to save catalog entry: {e}"),
    presets_load_failed: |e| format!("Could not read your presets: {e}"),
    presets_locked:
        "Presets are locked because the saved file could not be read. Move or fix it, then restart.",
    presets_save_failed: |e| format!("Failed to save presets: {e}"),
};

pub static ZH: Strings = Strings {
    settings: "设置",
    settings_tip: "主题、语言和自动色调 (Cmd+,)",
    settings_title: "设置",
    settings_theme: "主题",
    settings_language: "语言",
    settings_auto_tone: "自动色调",
    autotone_center_range: "居中整个范围",
    autotone_center_range_tip: "把直方图范围的中点放在 50%。保持照片的形状不变。",
    autotone_center_median: "居中中间像素",
    autotone_center_median_tip: "把一半照片比它暗的那个像素放在 50%。以暗部为主的照片会变得更亮。",
    form_general: "通用",
    theme_dark: "深色",
    theme_medium: "中灰",
    theme_light: "浅色",
    lang_english: "English",
    lang_chinese: "中文",

    open_folder: "打开\u{2026}",
    open_folder_tip: "打开其他文件夹 (Cmd+O)",
    back_to_grid_tip: "返回网格 (G)",
    help_tip: "键盘快捷键 (?)",
    browse_tab: "浏览",
    metadata_tab: "元数据",
    folders_tab_tip: "浏览文件夹 (I)",
    info_tab_tip: "照片元数据 (I)",
    info_no_selection: "未选择照片",
    info_file: "文件",
    info_camera: "相机",
    info_exposure: "曝光",
    info_date: "日期",
    info_location: "位置",
    info_name: "名称",
    info_size: "大小",
    info_dimensions: "尺寸",
    info_format: "格式",
    info_modified: "修改时间",
    info_make: "制造商",
    info_model: "型号",
    info_lens: "镜头",
    info_shutter: "快门",
    info_aperture: "光圈",
    info_iso: "感光度",
    info_focal_length: "焦距",
    info_exposure_comp: "曝光补偿",
    info_flash: "闪光灯",
    info_white_balance: "白平衡",
    info_captured: "拍摄时间",
    info_latitude: "纬度",
    info_longitude: "经度",
    info_altitude: "海拔",
    info_open_in_maps: "在地图中打开",
    flash_fired: "已闪光",
    flash_did_not_fire: "未闪光",
    wb_auto: "自动",
    wb_manual: "手动",
    landing_prompt: "选择一个照片文件夹开始",
    landing_tagline: "快速筛选、评分和冲印你的 RAW 照片。",
    landing_steps: [
        (
            "浏览",
            "以缩略图查看文件夹中的每张照片。按 Enter 打开大图。",
        ),
        ("评分", "按 0-5 打星，然后筛选图库，只留下你的精选。"),
        ("冲印", "调整影调和色彩，裁剪和旋转，然后按 X 导出 JPEG。"),
    ],
    landing_tip_title: "提示",
    landing_help: [
        "LightPhotos 从不修改你的原始照片。你的编辑保存在 .lightphotos 子文件夹中的小文件里。",
        "移动照片文件夹时，编辑也会随之移动。",
        "请不要重命名照片。编辑按文件名与照片对应。",
        "在应用中按 ? 查看所有键盘快捷键。",
    ],
    landing_allow_note: if WEB {
        "当浏览器提示\u{201C}Allow this site to edit files? \u{2026}\u{201D}时，\
         请点击允许 \u{2014} LightPhotos 需要它来读取照片并保存评分。"
    } else {
        ""
    },
    opening: "正在打开\u{2026}",
    choose_folder: "选择文件夹",
    reopen_session: "打开上次的文件夹",
    reopen_session_tip: |folder| format!("回到上次在 {folder} 的位置"),
    picker_title: "选择照片文件夹",

    rating_filter: "评分：",
    all: "全部",
    at_least_n_stars: "至少 N 星",
    exactly_n_stars: "正好 N 星",
    at_most_n_stars: "至多 N 星",
    show_rated: |cmp, n| format!("显示评分 {cmp} {n} 星的照片"),
    unrated: "未评分",
    unrated_tip: "只显示没有评分的照片",
    bursts: "连拍",
    bursts_needs_no_filter: "清除筛选后才能使用连拍",
    bursts_tip: "将连拍分组，并标记最清晰的一张 (B)",
    duplicates: "重复",
    duplicates_tip: "将外观相似的照片分组并加标记 (D)",
    eyes_closed: "闭眼",
    eyes_closed_tip: "只显示有人闭眼的照片",
    eyes_closed_needs_grouping: "打开连拍或重复后才能检测闭眼",
    n_photos: |n| format!("{n} 张照片"),

    no_selection: "未选择 \u{2014} 点击照片，或按 Cmd+A 全选",
    n_selected: |n| format!("已选 {n} 张"),
    rate_menu: "评分 \u{2605}",
    clear_rating: "清除评分",
    auto_tone: "自动色调",
    auto_tone_selection_tip: "根据每张所选照片自身的直方图设置色调滑块 (Cmd+Shift+U)",
    copy_settings: "拷贝设置",
    copy_settings_tip: "拷贝这张照片的调整设置 (Cmd+Shift+C)",
    copy_settings_needs_one: "只选择一张照片才能拷贝其设置",
    apply_settings: "应用设置",
    apply_settings_needs_copy: "请先从一张照片拷贝设置",
    preset_menu: "预设",
    apply_preset_selection_tip: "将已保存的预设应用到所选照片",
    settings_from: |name| format!("来自 {name}"),
    export_jpg: "导出",
    export_jpg_tip: "选择位置和尺寸，然后将所选照片导出为 JPG（X）",
    delete: "删除",
    delete_selection_tip: if WEB {
        "永久删除所选照片，无法撤销 (Delete)"
    } else {
        "将所选照片移到废纸篓 (Delete)"
    },

    confirm: "确认",
    cancel: "取消",
    shortcuts_title: "键盘快捷键",
    close: "关闭",
    help: &[
        HelpSection {
            title: "浏览",
            rows: &[
                ("Enter 或 Space", "打开所选照片（图库中）"),
                ("Esc", "返回图库（编辑器中）"),
                ("\u{2190} / \u{2192}", "上一张 / 下一张照片（编辑器中）"),
                (
                    "\u{2190} \u{2192} \u{2191} \u{2193}",
                    "在图库网格中移动选择",
                ),
                ("I", "在侧栏显示文件夹或照片信息"),
                ("?", "显示或隐藏此帮助"),
                (primary!("+,"), "设置：主题和语言"),
            ],
        },
        HelpSection {
            title: "选择",
            rows: &[
                (primary!("+A"), "全选当前文件夹"),
                ("Shift+点按", "连续选择"),
                (primary!("+点按"), "逐张加选或取消"),
                ("Shift+方向键", "扩展选择（图库）"),
            ],
        },
        HelpSection {
            title: "评分",
            rows: &[("0 1 2 3 4 5", "设置星级 0-5")],
        },
        HelpSection {
            title: "缩放和平移（编辑器中）",
            rows: &[
                ("Space", "循环缩放：适合、2x 适合、100%"),
                (primary!("+0", " 或 ", "+)"), "适合窗口"),
                (primary!("+1", " 或 ", "+!"), "100%（1:1 像素）"),
                (primary!("++", " 或 ", "+="), "放大（每次 20%）"),
                (primary!("+-"), "缩小（每次 20%）"),
                ("\u{2191} / \u{2193}", "放大 / 缩小（每次 10%）"),
                ("拖移", "平移"),
                ("Space+拖移", "平移"),
            ],
        },
        HelpSection {
            title: "编辑",
            rows: &[
                ("[", "向左旋转 90 度"),
                ("]", "向右旋转 90 度"),
                ("C", "裁剪"),
                ("Y", "调整前 / 调整后对比"),
                ("X", "打开或关闭导出表单"),
                (
                    "Delete",
                    if WEB {
                        "永久删除，无法撤销"
                    } else {
                        "移到废纸篓"
                    },
                ),
            ],
        },
        HelpSection {
            title: "主体",
            rows: if SUBJECT_KEYS {
                &[("O", "显示或隐藏主体"), ("Shift+O", "反选主体")]
            } else {
                &[]
            },
        },
        HelpSection {
            title: "修补",
            rows: &[
                ("K", "打开蒙版"),
                ("Q", "开启 / 关闭修补"),
                ("[ / ]", "画笔大小"),
                ("滚轮", "画笔大小"),
                ("Shift+[ / ]", "画笔羽化"),
                ("Shift+滚轮", "画笔羽化"),
                ("O", "显示或隐藏修补点"),
                ("Shift+O", "下一修补点"),
                ("Delete", "删除选中的修补点"),
                (primary!("+Z"), "撤销修补点"),
                ("Esc", "关闭修补"),
            ],
        },
    ],

    confirm_clear_rating: |n| format!("清除 {n} 张照片的评分？"),
    confirm_rate: |stars, n| format!("将 {n} 张照片评为 {stars}？"),
    confirm_apply_settings: |n| format!("将拷贝的设置应用到 {n} 张照片？"),
    confirm_apply_preset: |name, n| format!("将 {name} 应用到 {n} 张照片？"),
    confirm_auto_tone: |n| format!("对 {n} 张照片应用自动色调？"),
    confirm_delete: if WEB {
        |n| format!("永久删除 {n} 张照片？此操作无法撤销。")
    } else {
        |n| format!("将 {n} 张照片移到废纸篓？")
    },
    bulk_rate: "评分",
    bulk_apply_preset: "应用预设",
    bulk_delete: if WEB {
        "删除照片"
    } else {
        "移到废纸篓"
    },

    develop: "调整",
    tab_sliders: "调整",
    tab_crop: "裁剪",
    tab_masks: "蒙版",
    presets: "预设",
    save_preset_tip: "将当前设置保存为预设",
    import_lr_presets: "从 Lightroom 导入",
    import_lr_presets_tip: "导入 Lightroom 的 .xmp 预设文件",
    xmp_picker_title: "选择 Lightroom 预设",
    xmp_filter_name: "Lightroom 预设",
    preset_import_note: |list| format!("Lightroom 中以下设置无法导入：{list}"),
    preset_monochrome_note: "黑白效果以饱和度 -100 近似",
    no_presets: "尚无预设",
    apply_preset_tip: "应用到本张照片",
    preset_actions_tip: "重命名或删除",
    save_preset_title: "保存预设",
    rename_preset_title: "重命名预设",
    preset_name_hint: "预设名称",
    preset_name_label: "名称",
    save: "保存",
    rename: "重命名",
    preset_default_name: |n| format!("预设 {n}"),
    confirm_delete_preset: |name| format!("删除预设 {name}？"),
    reset: "复位",
    auto_tone_tip: "根据这张照片自身的直方图设置色调滑块",
    touch_up: "修补",
    brush_size: "大小",
    brush_size_tip: "[ / ] 或滚轮",
    feather: "羽化",
    feather_tip: "Shift+[ / Shift+] 或 Shift+滚轮",
    spots: "修补点：",
    pick_gray: "吸取灰点",
    pick_gray_tip: "点按图像中的一个中性灰像素",
    white_balance: "白平衡",
    tone: "影调",
    presence: "偏好",
    detail: "细节",
    temp: "色温",
    tint: "色调",
    exposure: "曝光度",
    contrast: "对比度",
    highlights: "高光",
    shadows: "阴影",
    whites: "白色色阶",
    blacks: "黑色色阶",
    vibrance: "自然饱和度",
    saturation: "饱和度",
    denoise: "降噪",

    before: "调整前",
    after: "调整后",
    invert: "反选",
    invert_tip: "高亮背景而不是主体",
    show_selection: "显示主体",
    selection_pending: "正在识别\u{2026}",
    no_subject: "未找到主体",
    show_selection_tip: "勾出 Vision 在这张照片中找到的主体",
    capture_date: |year, month, day, hour, minute| {
        format!("{year}年{month}月{day}日 {hour:02}:{minute:02}")
    },

    survey_heading: |n| format!("组内比较 \u{2014} {n} 张照片"),
    keep_best: "保留最佳，淘汰其余",
    keep_best_tip: "将最佳照片评为 \u{2605}5，组内其他照片评为 \u{2605}1 (Enter)",
    close_esc: "关闭 (Esc)",

    grid_title: |n| format!("网格  ({n} 张照片)"),
    survey_title: |n| format!("组内比较  ({n} 张照片)"),

    deleting: if WEB {
        |done, total| format!("正在删除 {done}/{total}\u{2026}")
    } else {
        |done, total| format!("正在移到废纸篓 {done}/{total}\u{2026}")
    },
    deleted: if WEB {
        |n| format!("已永久删除 {n} 张照片")
    } else {
        |n| format!("已将 {n} 张照片移到废纸篓")
    },
    deleted_partial: if WEB {
        |n, total, e| format!("已永久删除 {n}/{total} 张 \u{2014} 最后的错误：{e}")
    } else {
        |n, total, e| format!("已移动 {n}/{total} 张 \u{2014} 最后的错误：{e}")
    },
    delete_in_progress: "已有删除正在进行\u{2026}",
    delete_no_handle: |dir| format!("无法删除照片：{dir} 没有目录句柄"),
    copied_settings_from: |name| format!("已从 {name} 拷贝设置"),
    applied_settings: |n| format!("已将设置应用到 {n} 张照片"),
    saved_preset: |name| format!("已保存预设 {name}"),
    lr_import_status: |ok, failed, first| match (ok, failed) {
        (ok, 0) => format!("已导入 {ok} 个预设"),
        (0, _) => first.to_string(),
        (ok, failed) => format!("已导入 {ok} 个预设，跳过 {failed} 个（{first}）"),
    },
    lr_import_failed: |file, reason| format!("无法导入 {file}：{reason}"),
    applied_preset: |name, n| format!("已将 {name} 应用到 {n} 张照片"),
    renamed_preset: |name| format!("已重命名为 {name}"),
    deleted_preset: |name| format!("已删除预设 {name}"),
    cleared_rating: |n| format!("已清除 {n} 张照片的评分"),
    rated: |n, stars| format!("已将 {n} 张照片评为 \u{2605}{stars}"),
    kept_best: |n| format!("已保留最佳，其余 {n} 张评为 \u{2605}1"),
    export_title: |n| format!("导出 {n} 张照片"),
    export_destination: "目标",
    export_to_folder: "文件夹",
    export_to_immich: "Immich",
    export_exports_subfolder: "Exports 子文件夹",
    export_chosen_folder: "其他文件夹",
    export_choose_folder: "选择\u{2026}",
    export_size: "尺寸",
    export_folder: "位置",
    export_output: "输出",
    immich_account: "账户",
    export_size_full: "原始分辨率",
    export_size_long_edge: |px| format!("长边 {px} 像素"),
    export_run: "导出",
    export_needs_immich: "请先连接 Immich 服务器",
    immich_server_url: "服务器地址",
    immich_url_example: "示例：https://immich.gumnut.ai",
    immich_native_only: "上传到 Immich 仅在桌面版中可用。浏览器无法连接 Immich 服务器。",
    immich_api_key: "API 密钥",
    immich_connect: "连接",
    immich_connecting: "正在连接\u{2026}",
    immich_connected_as: |name| format!("已连接为 {name}"),
    immich_disconnect: "断开连接",
    immich_key_storage: if cfg!(target_os = "macos") {
        "密钥保存在你的钥匙串中。"
    } else {
        "密钥保存在只有你能读取的文件中。"
    },
    uploading: |done, total| format!("正在上传 {done}/{total}\u{2026}"),
    uploaded: |n, dup| match dup {
        0 => format!("已上传 {n} 张照片"),
        _ => format!("已上传 {n} 张照片，其中 {dup} 张已在服务器上"),
    },
    uploaded_partial: |ok, total, e| format!("已上传 {ok}/{total} 张 \u{2014} 最后的错误：{e}"),
    ratings_not_set: |summary, n, e| format!("{summary} \u{2014} {n} 张未能设置评分：{e}"),
    immich_album: "相册",
    album_none: "不放入相册",
    album_new: "新建相册\u{2026}",
    album_name: "相册名称",
    album_name_needed: "请先为新相册命名",
    albums_failed: |e| format!("无法加载相册：{e}"),
    adding_to_album: |summary, name| format!("{summary} \u{2014} 正在加入相册 {name}\u{2026}"),
    added_to_album: |summary, name| format!("{summary} \u{2014} 已加入相册 {name}"),
    album_failed: |summary, e| format!("{summary} \u{2014} 未能加入相册：{e}"),
    export_nothing_selected: "导出：没有选择任何照片",
    export_in_progress: "导出正在进行\u{2026}",
    export_catalog_loading: "导出：目录仍在加载，请稍后再试\u{2026}",
    export_no_handle: "导出：当前文件夹没有目录句柄",
    export_no_folder: |e| format!("导出失败：无法创建 Exports 文件夹：{e}"),
    exporting: |done, total| format!("正在导出 {done}/{total}\u{2026}"),
    exported: |n| format!("已导出 {n} 张照片"),
    exported_partial: |ok, total, e| format!("已导出 {ok}/{total} 张 \u{2014} 最后的错误：{e}"),
    auto_tone_needs_load: "照片加载完成后才能自动色调",
    auto_tone_applied: "已应用自动色调",
    auto_tone_waits_for_catalog: "目录加载完成后将开始自动色调\u{2026}",
    auto_tone_stopped: |done, total| format!("自动色调已停止于 {done}/{total}"),
    auto_tone_applied_n: |n| format!("已对 {n} 张照片应用自动色调"),
    auto_tone_progress: |done, total| format!("自动色调 {done}/{total}\u{2026}"),
    touch_up_limit: "修补最多支持 64 个点",
    touch_up_needs_full: "修补需要全分辨率图像",
    wb_no_image: "没有已加载的图像可供取样",
    wb_pick_brighter: "请选择更亮、饱和度更低的像素来设置白平衡",
    open_folder_failed: |e| format!("无法打开文件夹：{e}"),
    preview_failed: |path| format!("无法加载 {path} 的预览"),
    folder_handle_missing: |dir| format!("无法打开 {dir} \u{2014} 缺少文件夹句柄"),
    list_folder_failed: |dir, e| format!("无法列出 {dir}：{e}"),
    catalog_save_failed: |e| format!("无法保存目录条目：{e}"),
    presets_load_failed: |e| format!("无法读取预设：{e}"),
    presets_locked: "预设已锁定，因为无法读取已保存的文件。请将其移走或修复，然后重新启动。",
    presets_save_failed: |e| format!("无法保存预设：{e}"),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_map_to_languages() {
        for (tag, want) in [
            ("zh-Hans-CN", Some(Lang::Zh)),
            ("zh-Hant-TW", Some(Lang::Zh)),
            ("zh_CN.UTF-8", Some(Lang::Zh)),
            ("ZH", Some(Lang::Zh)),
            ("en-US", Some(Lang::En)),
            ("en_GB.UTF-8", Some(Lang::En)),
            ("fr-FR", None),
            ("C", None),
            ("", None),
        ] {
            assert_eq!(Lang::from_tag(tag), want, "{tag:?}");
        }
    }

    #[test]
    fn stored_codes_round_trip() {
        for lang in [Lang::En, Lang::Zh] {
            assert_eq!(Lang::from_tag(lang.code()), Some(lang));
        }
    }

    #[test]
    fn language_names_are_the_same_in_both_catalogs() {
        assert_eq!(EN.lang_english, ZH.lang_english);
        assert_eq!(EN.lang_chinese, ZH.lang_chinese);
    }

    #[test]
    fn capture_dates_follow_each_language() {
        assert_eq!(
            (EN.capture_date)(2026, 7, 14, 15, 42),
            "Jul 14, 2026 3:42 PM"
        );
        assert_eq!((EN.capture_date)(2026, 1, 1, 0, 5), "Jan 1, 2026 12:05 AM");
        assert_eq!((EN.capture_date)(2026, 1, 1, 12, 0), "Jan 1, 2026 12:00 PM");
        assert_eq!(
            (ZH.capture_date)(2026, 7, 14, 15, 42),
            "2026年7月14日 15:42"
        );
        assert_eq!((ZH.capture_date)(2026, 1, 1, 0, 5), "2026年1月1日 00:05");
    }

    /// Same rule as `scripts/subset-cjk-font.sh`.
    fn is_cjk(c: char) -> bool {
        matches!(c as u32, 0x2E80..=0x9FFF | 0xFF00..=0xFFEF)
    }

    /// Outside macOS the UI's Chinese can come only from the bundled subset
    /// (no system Chinese font, or the web before the full font arrives), so
    /// a character it lacks renders as a box there.
    #[test]
    fn bundled_font_covers_every_cjk_character() {
        use skrifa::MetadataProvider as _;
        let font = skrifa::FontRef::new(include_bytes!("../assets/fonts/NotoSansSC-ui-subset.otf"))
            .unwrap();
        let charmap = font.charmap();
        let missing: String = include_str!("i18n.rs")
            .chars()
            .filter(|&c| is_cjk(c) && charmap.map(c).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "run scripts/subset-cjk-font.sh; the bundled font lacks {missing:?}"
        );
    }

    /// String literals passed straight to a text widget or status message,
    /// which would show English in every language.
    fn untranslated(file: &str, src: &str) -> Vec<String> {
        const SINKS: &[&str] = &[
            ".button(",
            ".label(",
            ".heading(",
            ".weak(",
            ".strong(",
            "on_hover_text(",
            "on_disabled_hover_text(",
            "selectable_label(",
            "Button::new(",
            "Button::selectable(",
            "RichText::new(",
            "selected_text(",
            "set_status(",
            "set_title(",
        ];
        const BRAND: &str = "\"LightPhotos\"";
        let code: String = src
            .lines()
            .take_while(|l| !l.contains("#[cfg(test)]"))
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut found = Vec::new();
        for sink in SINKS {
            for (start, _) in code.match_indices(sink) {
                let args = &code[start + sink.len()..];
                let mut depth = 1;
                let end = args
                    .char_indices()
                    .find(|&(_, c)| {
                        depth += match c {
                            '(' => 1,
                            ')' => -1,
                            _ => 0,
                        };
                        depth == 0
                    })
                    .map_or(args.len(), |(i, _)| i);
                for lit in args[..end].split('"').skip(1).step_by(2) {
                    let english = lit
                        .as_bytes()
                        .windows(2)
                        .any(|w| w.iter().all(u8::is_ascii_alphabetic));
                    if english && format!("\"{lit}\"") != BRAND {
                        found.push(format!("{file}: {sink}\"{lit}\""));
                    }
                }
            }
        }
        found
    }

    #[test]
    fn ui_text_goes_through_the_catalog() {
        macro_rules! sources {
            ($($path:literal),* $(,)?) => {
                [$(($path, include_str!($path))),*]
            };
        }
        let files = sources!(
            "dialog.rs",
            "ui/mod.rs",
            "ui/develop_panel.rs",
            "ui/grid.rs",
            "ui/info_panel.rs",
            "ui/loupe.rs",
            "ui/modals.rs",
            "ui/survey.rs",
            "ui/toolbar.rs",
            "app/mod.rs",
            "app/accessors.rs",
            "app/adjust.rs",
            "app/autotone.rs",
            "app/catalog.rs",
            "app/crop.rs",
            "app/export.rs",
            "app/histogram.rs",
            "app/keys.rs",
            "app/loupe.rs",
            "app/nav.rs",
            "app/presets.rs",
            "app/thumbs.rs",
            "app/web.rs",
        );
        let found: Vec<String> = files
            .iter()
            .flat_map(|(file, src)| untranslated(file, src))
            .collect();
        assert!(
            found.is_empty(),
            "add these to i18n::Strings:\n{}",
            found.join("\n")
        );
    }

    /// The scan must catch a literal in a multi-line call, or it proves nothing.
    #[test]
    fn untranslated_scan_flags_literals() {
        let src = "ui.button(\"Keep\").clicked();\nself.set_status(format!(\n    \"Moved {n}\"\n));\nui.heading(\"LightPhotos\");\n// ui.label(\"a comment\");\nui.label(t().close);";
        assert_eq!(
            untranslated("x.rs", src),
            ["x.rs: .button(\"Keep\"", "x.rs: set_status(\"Moved {n}\"",]
        );
    }

    /// Both languages list the same shortcuts in the same order, so the
    /// overlay can't drift between them.
    #[test]
    fn help_tables_have_the_same_shape() {
        assert_eq!(EN.help.len(), ZH.help.len());
        for (en, zh) in EN.help.iter().zip(ZH.help) {
            assert_eq!(en.rows.len(), zh.rows.len(), "section {:?}", en.title);
        }
    }
}
