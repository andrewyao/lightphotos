// SPDX-License-Identifier: MIT OR Apache-2.0

//! User-facing text in every supported language. Each language is one
//! `Strings` value, so a string missing from any language is a compile error.
//! Log lines, CLI output, and OS error details stay English.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::app::SHOW_AUTOTONE_CENTERING;

/// Whether Settings has the RAW tone curve choice, which only macOS's
/// CoreImage decode offers.
const RAW_SETTING: bool = cfg!(target_os = "macos");
use crate::develop::{Section, SliderId};

mod de;
mod en;
mod fr;
mod ja;
mod ko;
mod zh_hans;
mod zh_hant;

pub use de::DE;
pub use en::EN;
pub use fr::FR;
pub use ja::JA;
pub use ko::KO;
pub use zh_hans::ZH_HANS;
pub use zh_hant::ZH_HANT;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    ZhHans,
    ZhHant,
    Ja,
    Ko,
    De,
    Fr,
}

impl Lang {
    /// Every language, in the order Settings lists them.
    pub const ALL: [Lang; 7] = [
        Lang::En,
        Lang::ZhHans,
        Lang::ZhHant,
        Lang::Ja,
        Lang::Ko,
        Lang::De,
        Lang::Fr,
    ];

    /// The language's own name, so a reader of any language can find theirs.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhHans => "简体中文",
            Lang::ZhHant => "繁體中文",
            Lang::Ja => "日本語",
            Lang::Ko => "한국어",
            Lang::De => "Deutsch",
            Lang::Fr => "Français",
        }
    }

    /// The code saved in prefs. Never change one: an old install reads it back.
    fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhHans => "zh-Hans",
            Lang::ZhHant => "zh-Hant",
            Lang::Ja => "ja",
            Lang::Ko => "ko",
            Lang::De => "de",
            Lang::Fr => "fr",
        }
    }

    /// Parses a stored code, a BCP 47 tag such as `zh-Hant-TW`, or a POSIX
    /// locale such as `zh_TW.UTF-8`. A Chinese tag is Traditional when its
    /// script is Hant or, with no script, its region is Taiwan, Hong Kong or
    /// Macau; otherwise it is Simplified, as is the bare `zh` an earlier
    /// version saved.
    fn from_tag(tag: &str) -> Option<Lang> {
        let mut parts = tag.split(['-', '_', '.']).map(str::to_ascii_lowercase);
        match parts.next()?.as_str() {
            "en" => Some(Lang::En),
            "ja" => Some(Lang::Ja),
            "ko" => Some(Lang::Ko),
            "de" => Some(Lang::De),
            "fr" => Some(Lang::Fr),
            "zh" => {
                let rest: Vec<String> = parts.collect();
                let has = |want: &[&str]| rest.iter().any(|p| want.contains(&p.as_str()));
                if !has(&["hans"]) && has(&["hant", "tw", "hk", "mo"]) {
                    Some(Lang::ZhHant)
                } else {
                    Some(Lang::ZhHans)
                }
            }
            _ => None,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    Lang::ALL
        .get(usize::from(LANG.load(Ordering::Relaxed)))
        .copied()
        .unwrap_or(Lang::En)
}

fn set_lang(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

/// The current language's strings.
pub fn t() -> &'static Strings {
    match lang() {
        Lang::En => &EN,
        Lang::ZhHans => &ZH_HANS,
        Lang::ZhHant => &ZH_HANT,
        Lang::Ja => &JA,
        Lang::Ko => &KO,
        Lang::De => &DE,
        Lang::Fr => &FR,
    }
}

/// `prefs` key holding the saved language code.
const LANGUAGE_KEY: &str = "language";

/// Pick the startup language: a `?lang=` in the page's URL (the browser build,
/// opened from the site), else the saved choice, else the OS or browser
/// language, else English. The URL's language is not saved; only a pick in
/// Settings is.
pub fn init() {
    let lang = lang_from_query(&location_search())
        .or_else(saved_lang)
        .or_else(|| system_tags().iter().find_map(|t| Lang::from_tag(t)))
        .unwrap_or(Lang::En);
    set_lang(lang);
}

/// The language a query string such as `?lang=de&x=1` asks for. A language we
/// don't have is no request, so startup falls through to the next source.
fn lang_from_query(search: &str) -> Option<Lang> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("lang="))
        .and_then(Lang::from_tag)
}

#[cfg(target_arch = "wasm32")]
fn location_search() -> String {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn location_search() -> String {
    String::new()
}

fn saved_lang() -> Option<Lang> {
    Lang::from_tag(crate::persist::prefs::load(LANGUAGE_KEY)?.trim())
}

/// Switch language and remember the choice for the next launch. A storage
/// failure costs one relaunch's language, so it is logged, not surfaced.
pub fn choose(lang: Lang) {
    set_lang(lang);
    if let Err(e) = crate::persist::prefs::save(LANGUAGE_KEY, lang.code()) {
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

/// The help and tooltips write the command key as "Cmd". `keys` swaps it
/// for "Ctrl" where Ctrl is the command key, at draw time, because a browser
/// learns whether it runs on a Mac only at runtime.
pub fn keys(text: &str) -> std::borrow::Cow<'_, str> {
    if command_is_cmd() || !text.contains("Cmd+") {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.replace("Cmd+", "Ctrl+"))
    }
}

/// Whether the command key is Cmd: in the macOS app, and in a browser on a
/// Mac. `App::handle_key` accepts either key everywhere.
fn command_is_cmd() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        crate::app::browser_is_mac()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        cfg!(target_os = "macos")
    }
}

const WEB: bool = cfg!(target_arch = "wasm32");

// wasm32 lacks the folder picker, Lightroom preset import, and Immich export,
// so their strings go unread there.
#[cfg_attr(
    target_arch = "wasm32",
    allow(
        dead_code,
        reason = "the web build has no folder picker, Lightroom import or Immich"
    )
)]
pub struct Strings {
    // Settings dialog.
    pub settings: &'static str,
    pub settings_tip: &'static str,
    pub settings_title: &'static str,
    pub settings_theme: &'static str,
    pub settings_language: &'static str,
    pub settings_auto_tone: &'static str,
    pub settings_raw: &'static str,
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "a macOS-only setting")
    )]
    pub apple_tone_curve: &'static str,
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "a macOS-only setting")
    )]
    pub apple_tone_curve_hint: &'static str,
    pub autotone_center_range: &'static str,
    pub autotone_center_range_tip: &'static str,
    pub autotone_center_median: &'static str,
    pub autotone_center_median_tip: &'static str,
    pub form_general: &'static str,
    pub theme_dark: &'static str,
    pub theme_medium: &'static str,
    pub theme_light: &'static str,

    // Header and landing page.
    pub open_folder: &'static str,
    pub open_folder_tip: &'static str,
    pub back_to_grid_tip: &'static str,
    pub fullscreen_tip: &'static str,
    pub slideshow_menu_tip: &'static str,
    pub slideshow_play: &'static str,
    pub slideshow_duration: &'static str,
    pub slideshow_random: &'static str,
    pub slideshow_fill: &'static str,
    pub seconds_suffix: &'static str,
    /// Hover text for the signal bars beside the Loupe's file name, one per decode.
    pub shown_thumb_tip: &'static str,
    pub shown_embedded_tip: &'static str,
    pub shown_preview_tip: &'static str,
    pub shown_full_tip: &'static str,
    pub help_tip: &'static str,
    pub landing_prompt: &'static str,
    /// One line under the prompt saying what the app is for.
    pub landing_tagline: &'static str,
    /// Web only: the File System Access permission the browser shows once a
    /// folder is picked. Empty off the web, where the picker is native.
    pub landing_allow_note: &'static str,
    pub opening: &'static str,
    pub picker_title: &'static str,
    /// The Folders panel's button over the list, once it lists a folder.
    pub add_folder: &'static str,
    pub add_folder_tip: &'static str,
    /// A Folders root's right-click item. The folder's files stay.
    pub remove_folder: &'static str,
    /// Hover text on a Folders root that isn't there, such as on an
    /// unplugged drive. Takes the folder's path.
    pub folder_missing: fn(&str) -> String,
    /// The same on the web, for a root whose access this browser no longer
    /// keeps. Takes the folder's name.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub folder_not_kept: fn(&str) -> String,
    /// The home page's empty actions bar.
    pub select_photos_hint: &'static str,

    // The guided tour.
    pub tour: &'static str,
    pub tour_tip: &'static str,
    /// The home page's button under Open Folder.
    pub take_tour: &'static str,
    pub skip_tour: &'static str,
    pub tour_back: &'static str,
    pub tour_next: &'static str,
    pub tour_done: &'static str,
    /// "2 of 6" under a step.
    pub tour_step_of: fn(usize, usize) -> String,
    /// One (title, body) per stop, in `ui::tour::TourStep::ALL` order.
    pub tour_steps: [(&'static str, &'static str); 9],
    /// What the Loupe's own rail icons do, under its tour stop: Crop,
    /// Cleanup and Compare, in that order. Their names are the rail's tips.
    pub tour_loupe_icons: [&'static str; 3],

    // Left panel.
    pub metadata_title: &'static str,
    pub info_tab_tip: &'static str,
    pub info_no_selection: &'static str,
    pub info_file: &'static str,
    pub info_camera: &'static str,
    pub info_exposure: &'static str,
    pub info_date: &'static str,
    pub info_location: &'static str,
    pub info_name: &'static str,
    pub info_dimensions: &'static str,
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
    pub unrated_tip: &'static str,
    pub flag_filter_label: &'static str,
    /// Hover text on a flag filter icon; takes the flag state's name.
    pub show_flagged: fn(&str) -> String,
    pub label_filter_label: &'static str,
    /// Hover text on a color label filter dot; takes the label's name.
    pub show_labeled: fn(&str) -> String,
    pub flag_not_rejected: &'static str,
    pub flag_picked: &'static str,
    pub flag_rejected: &'static str,
    pub flag_unflagged: &'static str,
    /// Hover text on a flag button; takes the flag's name.
    pub set_flag_tip: fn(&str) -> String,
    pub eyes_closed: &'static str,
    pub eyes_closed_tip: &'static str,
    pub n_of_m_photos: fn(usize, usize) -> String,
    /// The grid's message when the filters hide every photo.
    pub no_filter_matches: &'static str,
    pub show_all_photos: &'static str,
    /// The grid cell's label for a photo whose thumbnail failed to decode.
    pub thumb_unreadable: &'static str,

    // Selection bar.
    pub selected_photo: &'static str,
    pub selected_n_photos: fn(usize) -> String,
    pub score_all: &'static str,
    pub score_all_tip: &'static str,
    pub group_all_bursts: &'static str,
    pub group_all_bursts_tip: &'static str,
    pub group_selected_bursts_tip: &'static str,
    pub expand_stack: &'static str,
    pub collapse_stack: &'static str,
    pub auto_adjust_all: &'static str,
    pub auto_adjust_all_tip: &'static str,
    pub update_score: &'static str,
    pub n_selected: fn(usize) -> String,
    pub rate_menu: &'static str,
    /// Names each toolbar's Actions menu; the toolbar adds the arrow.
    pub actions_menu: &'static str,
    /// Items of the menu a right click on a photo opens. The rest of that
    /// menu shares the menu bar's strings.
    pub open_in_loupe: &'static str,
    pub group_into_stack: &'static str,
    pub reset_adjustments: &'static str,
    /// Names the score in the filmstrip bar, before its value or range.
    pub score_label: &'static str,
    /// Heads the Actions menu over several selected photos.
    pub selected_n_photos_title: fn(usize) -> String,
    pub update_scores: &'static str,
    pub clear_rating: &'static str,
    pub auto_tone: &'static str,
    pub auto_tone_selection_tip: &'static str,
    pub score_selection_tip: &'static str,
    pub quality_of: fn(u8) -> String,
    pub base_technical: fn(u8) -> String,
    pub base_aesthetics: fn(u8) -> String,
    pub score_no_breakdown: &'static str,
    pub no_penalties: &'static str,
    pub penalty: fn(crate::scoring::quality::Penalty) -> &'static str,
    pub score_stale: &'static str,
    pub sort_by: &'static str,
    pub sort_name: &'static str,
    pub sort_time: &'static str,
    pub zoom_fit: &'static str,
    pub zoom_tip: &'static str,
    pub zoom_actual: &'static str,
    pub sort_time_tip: &'static str,
    pub sort_modified: &'static str,
    pub sort_modified_tip: &'static str,
    pub sort_size: &'static str,
    pub sort_size_tip: &'static str,
    pub sort_shutter: &'static str,
    pub sort_shutter_tip: &'static str,
    pub sort_quality: &'static str,
    pub sort_quality_tip: &'static str,
    pub sort_rating: &'static str,
    pub sort_rating_tip: &'static str,
    pub sort_edited: &'static str,
    pub sort_edited_tip: &'static str,
    pub sort_ascending_tip: &'static str,
    pub sort_descending_tip: &'static str,
    pub copy_settings: &'static str,
    pub copy_settings_tip: &'static str,
    /// The cog beside Copy Adjustment, which opens Copy Settings.
    pub copy_settings_choose_tip: &'static str,
    /// Copy Settings' line over its checkboxes.
    pub copy_settings_prompt: &'static str,
    pub copy_button: &'static str,
    pub check_all: &'static str,
    pub check_none: &'static str,
    pub compare_too_many: &'static str,
    pub effects: &'static str,
    pub crop_section: &'static str,
    pub apply_settings: &'static str,
    pub apply_settings_needs_copy: &'static str,
    pub apply_settings_tip: &'static str,
    pub settings_from: fn(&str) -> String,
    pub export_jpg: &'static str,
    pub export_jpg_tip: &'static str,
    pub bursts_reading: &'static str,
    pub bursts_none: &'static str,
    pub bursts_grouped: fn(usize, usize) -> String,
    pub delete: &'static str,
    pub delete_selection_tip: &'static str,

    // Modals.
    pub confirm: &'static str,
    pub cancel: &'static str,
    pub shortcuts_title: &'static str,
    pub close: &'static str,
    pub help: &'static [HelpSection],

    // Bulk confirmations.
    pub confirm_apply_settings: fn(usize) -> String,
    /// (preset name, photo count)
    pub confirm_auto_tone: fn(usize) -> String,
    pub confirm_delete: fn(usize) -> String,
    pub confirm_delete_groups: fn(usize, usize) -> String,
    pub delete_group_title: &'static str,
    pub delete_group_prompt: fn(usize, usize) -> String,
    pub remove_group: &'static str,
    pub trash_group_photos: fn(usize) -> String,
    /// Titles and confirm buttons of the bulk dialogs that no toolbar label fits.
    pub bulk_delete: &'static str,

    // Develop panel.
    pub develop: &'static str,
    pub tab_sliders: &'static str,
    pub tab_crop: &'static str,
    pub tab_masks: &'static str,
    pub crop_rotate: &'static str,
    pub crop_left: &'static str,
    pub crop_right: &'static str,
    pub crop_size: &'static str,
    pub crop_overlay: &'static str,
    pub crop_thirds: &'static str,
    pub crop_grid: &'static str,
    pub crop_golden: &'static str,
    pub crop_diagonal: &'static str,
    pub crop_spiral: &'static str,
    pub crop_overlay_none: &'static str,
    pub crop_aspect: &'static str,
    pub crop_original: &'static str,
    pub crop_custom: &'static str,
    pub crop_orientation: &'static str,
    pub crop_horizontal: &'static str,
    pub crop_vertical: &'static str,
    pub crop_straighten: &'static str,
    pub crop_straighten_tool: &'static str,
    pub crop_straighten_hint: &'static str,
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
    pub search_presets: &'static str,
    pub my_presets: &'static str,
    pub no_matching_presets: &'static str,
    pub save_current_as_preset: &'static str,
    pub add_preset_folder: &'static str,
    pub add_preset_folder_tip: &'static str,
    pub preset_folder_picker_title: &'static str,
    pub preset_folders: &'static str,
    pub remove: &'static str,
    /// (preset folder name)
    pub reading_presets: fn(&str) -> String,
    /// (preset folder name)
    pub preset_folder_unavailable: fn(&str) -> String,
    /// (preset folder name)
    pub preset_folder_empty: fn(&str) -> String,
    /// (preset folder name)
    pub preset_folder_needs_permission: fn(&str) -> String,
    /// (preset folder name, how many files were read)
    pub preset_folder_truncated: fn(&str, usize) -> String,
    /// (number of .xmp files skipped)
    pub presets_skipped: fn(usize) -> String,
    pub presets_skipped_tip: &'static str,
    /// (why)
    pub preset_folder_add_failed: fn(&str) -> String,
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
    pub black_and_white: &'static str,
    pub black_and_white_tip: &'static str,
    pub touch_up: &'static str,
    pub brush_size: &'static str,
    pub brush_size_tip: &'static str,
    pub feather: &'static str,
    pub feather_tip: &'static str,
    pub opacity: &'static str,
    pub opacity_tip: &'static str,
    /// The mouse shortcuts, shown under the Size and Feather sliders.
    pub spots: &'static str,
    pub pick_gray: &'static str,
    pub pick_gray_tip: &'static str,
    pub white_balance: &'static str,
    pub tone: &'static str,
    pub presence: &'static str,
    pub detail: &'static str,
    pub optics: &'static str,
    pub vignette: &'static str,
    pub grain: &'static str,
    pub curve: &'static str,
    pub point_curve: &'static str,
    pub curve_linear: &'static str,
    pub curve_medium_contrast: &'static str,
    pub curve_strong_contrast: &'static str,
    pub curve_custom: &'static str,
    pub curve_rgb: &'static str,
    pub curve_red: &'static str,
    pub curve_green: &'static str,
    pub curve_blue: &'static str,
    pub color_mixer: &'static str,
    pub gray_mix: &'static str,
    pub copy_color: &'static str,
    pub color_grading: &'static str,
    pub midtones: &'static str,
    pub global: &'static str,
    pub blending: &'static str,
    pub balance: &'static str,
    pub mix_hue: &'static str,
    pub mix_luminance: &'static str,
    /// The Color Mixer's bands, in `develop::Band` order.
    pub bands: [&'static str; 8],
    pub remove_ca: &'static str,
    pub remove_ca_tip: &'static str,
    pub no_photo_selected: &'static str,
    pub multiple_photos_selected: &'static str,
    pub step_big_down: &'static str,
    pub step_down: &'static str,
    pub step_reset: &'static str,
    pub step_up: &'static str,
    pub step_big_up: &'static str,
    pub temp: &'static str,
    pub tint: &'static str,
    pub exposure: &'static str,
    pub contrast: &'static str,
    pub highlights: &'static str,
    pub shadows: &'static str,
    pub whites: &'static str,
    pub blacks: &'static str,
    pub texture: &'static str,
    pub clarity: &'static str,
    pub dehaze: &'static str,
    pub vibrance: &'static str,
    pub saturation: &'static str,
    pub denoise: &'static str,
    pub sharpening: &'static str,
    pub sharpen_radius: &'static str,
    pub sharpen_detail: &'static str,
    pub sharpen_masking: &'static str,
    pub amount: &'static str,
    pub midpoint: &'static str,
    pub roundness: &'static str,
    pub grain_size: &'static str,
    pub roughness: &'static str,

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

    // Window titles.

    // Status messages.
    pub deleting: fn(usize, usize) -> String,
    pub deleted: fn(usize) -> String,
    pub deleted_partial: fn(usize, usize, &str) -> String,
    pub delete_in_progress: &'static str,
    pub group_refused_loading: &'static str,
    pub group_name_unsaveable: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
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
    pub flagged: fn(usize, &str) -> String,
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
    pub export_file: &'static str,
    pub export_format: &'static str,
    pub export_format_tiff16: &'static str,
    pub export_format_webp: &'static str,
    pub export_limit: &'static str,
    pub export_limit_none: &'static str,
    pub export_limit_kb: fn(u32) -> String,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_preset: &'static str,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_fastest: &'static str,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_baseline: &'static str,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_progressive: &'static str,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_smallest: &'static str,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "ImageIO's JPEG encoder has no presets")
    )]
    pub export_jpeg_preset_hint: &'static str,
    pub export_naming: &'static str,
    pub export_naming_hint: &'static str,
    pub export_naming_example: fn(&str) -> String,
    pub export_metadata: &'static str,
    pub export_metadata_include: &'static str,
    pub export_keep_location: &'static str,
    pub export_keep_identity: &'static str,
    pub export_keep_hint: &'static str,
    pub export_copyright: &'static str,
    pub export_copyright_hint: &'static str,
    pub export_sharpen: &'static str,
    pub export_sharpen_off: &'static str,
    pub export_sharpen_screen: &'static str,
    pub export_sharpen_matte: &'static str,
    pub export_sharpen_glossy: &'static str,
    pub export_sharpen_low: &'static str,
    pub export_sharpen_standard: &'static str,
    pub export_sharpen_high: &'static str,
    pub export_run: &'static str,
    pub export_needs_immich: &'static str,
    pub immich_server_url: &'static str,
    pub immich_url_example: &'static str,
    pub immich_plain_http: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
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
    pub export_edits_loading: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub export_no_handle: &'static str,
    pub export_no_folder: fn(&str) -> String,
    pub exporting: fn(usize, usize) -> String,
    pub exported: fn(usize) -> String,
    pub exported_partial: fn(usize, usize, &str) -> String,
    pub auto_tone_needs_load: &'static str,
    pub auto_tone_applied: &'static str,
    pub remove_ca_measuring: &'static str,
    pub remove_ca_measuring_n: fn(usize, usize) -> String,
    pub remove_ca_applied: &'static str,
    pub remove_ca_failed: &'static str,
    pub auto_tone_waits_for_edits: &'static str,
    pub auto_tone_stopped: fn(usize, usize) -> String,
    pub auto_tone_applied_n: fn(usize) -> String,
    pub auto_tone_progress: fn(usize, usize) -> String,
    pub scoring_progress: fn(usize, usize) -> String,
    pub scoring_stopped: fn(usize, usize) -> String,
    pub scored_n: fn(usize) -> String,
    pub scored_partial: fn(usize, usize) -> String,
    pub touch_up_limit: &'static str,
    pub mask_limit: &'static str,
    pub masks: &'static str,
    pub linear_gradient: &'static str,
    pub radial_gradient: &'static str,
    pub linear_gradient_tip: &'static str,
    pub radial_gradient_tip: &'static str,
    pub brush_mask: &'static str,
    pub brush_mask_tip: &'static str,
    pub brush_paint_hint: &'static str,
    pub flow: &'static str,
    pub flow_tip: &'static str,
    pub brush_erase: &'static str,
    pub brush_erase_tip: &'static str,
    pub brush_point_limit: &'static str,
    pub brush_stroke_limit: &'static str,
    pub mask_invert: &'static str,
    pub mask_draw_hint: &'static str,
    pub subject_mask: &'static str,
    pub sky_mask: &'static str,
    pub detect_nothing_found: fn(&str) -> String,
    pub detect_failed: fn(&str, &str) -> String,
    pub subject_mask_tip: &'static str,
    pub sky_mask_tip: &'static str,
    pub detect_needs_mac: &'static str,
    pub detect_unavailable: &'static str,
    pub detect_file_missing: &'static str,
    pub detect_read_failed: fn(&str, &str) -> String,
    pub mask_detecting: &'static str,
    pub mask_retry: &'static str,
    pub export_masks_detecting: &'static str,
    pub touch_up_needs_full: &'static str,
    pub wb_no_image: &'static str,
    pub wb_pick_brighter: &'static str,
    pub open_folder_failed: fn(&str) -> String,
    /// The last catalog could not open at launch, so the default one did.
    /// Takes why.
    pub catalog_fallback: fn(&str) -> String,
    /// A catalog the user chose could not open. Takes why.
    pub catalog_open_failed: fn(&str) -> String,
    /// A catalog file could not be written. Takes why.
    pub catalog_save_failed: fn(&str) -> String,
    /// (the file or catalog, and why)
    pub preset_folders_unreadable: fn(&str) -> String,
    /// New Catalog… could not make the catalog. Takes why.
    pub catalog_create_failed: fn(&str) -> String,
    /// Hover text on the Folders heading, which names the catalog.
    pub catalog_tip: &'static str,
    /// The Folders heading's dropdown items.
    pub new_catalog: &'static str,
    pub open_catalog: &'static str,
    /// The save dialog's title, the name it fills in (the extension is
    /// added), its file-type filter, and the Open Catalog picker's title.
    pub catalog_save_title: &'static str,
    pub catalog_untitled: &'static str,
    pub catalog_filter_name: &'static str,
    pub catalog_picker_title: &'static str,
    /// The browser's New Catalog prompt: its field's label and hint, and
    /// the button that makes the catalog.
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub catalog_name_label: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub catalog_name_hint: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub create: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub preview_failed: fn(&str) -> String,
    /// The group tiles' page, as first, last and group size.
    pub group_page: fn(usize, usize, usize) -> String,
    pub tile_load: &'static str,
    pub tile_speed: &'static str,
    /// Names its cost, so the person knows why the tiles take a moment.
    pub tile_full: &'static str,
    pub set_as_rep: &'static str,
    pub representative: &'static str,
    /// Hover text on the info bar's icon for the Loupe alone.
    /// Hover text on the info bar's icon for the Compare pane.
    pub view_compare: &'static str,
    pub view_compare_needs_group: &'static str,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub folder_handle_missing: fn(&str) -> String,
    #[cfg_attr(
        not(target_arch = "wasm32"),
        allow(dead_code, reason = "a message only the browser build shows")
    )]
    pub list_folder_failed: fn(&str, &str) -> String,
    pub edits_save_failed: fn(&str) -> String,
    /// Who holds a folder that opened read-only: another LightPhotos.
    pub held_by_app: fn(u32) -> String,
    /// ... a headless lightphotos-mcp.
    pub held_by_mcp: fn(u32) -> String,
    /// ... a holder the lock file doesn't name.
    pub held_by_other: &'static str,
    /// The read-only folder's line and every refusal, with who holds it.
    pub folder_read_only: fn(&str) -> String,
    /// A folder whose lock couldn't be made, so it opened read-only.
    pub folder_cant_lock: &'static str,
    /// A folder whose sidecars couldn't be read at open.
    pub folder_no_access: &'static str,
    /// A folder on a volume without file locks.
    pub folder_lockless: &'static str,
    /// An edit while the folder is still opening.
    pub folder_opening: &'static str,
    pub presets_load_failed: fn(&str) -> String,
    pub presets_locked: &'static str,
    pub presets_save_failed: fn(&str) -> String,

    // Native menu bar.
    pub menu: MenuStrings,
}

/// Titles and items of the macOS menu bar, which a photo's right-click
/// menu borrows on every platform.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only the macOS menu bar reads them")
)]
pub struct MenuStrings {
    pub about: &'static str,
    pub settings: &'static str,
    pub hide: &'static str,
    pub hide_others: &'static str,
    pub show_all: &'static str,
    pub quit: &'static str,
    pub file: &'static str,
    pub new_catalog: &'static str,
    pub open_catalog: &'static str,
    pub open_folder: &'static str,
    pub export: &'static str,
    pub edit: &'static str,
    pub undo: &'static str,
    pub cut: &'static str,
    pub copy: &'static str,
    pub paste: &'static str,
    pub select_all: &'static str,
    pub deselect_all: &'static str,
    pub copy_settings: &'static str,
    pub paste_settings: &'static str,
    pub photo: &'static str,
    pub rate: &'static str,
    pub no_rating: &'static str,
    pub set_flag: &'static str,
    pub flag_picked: &'static str,
    pub flag_unflagged: &'static str,
    pub flag_rejected: &'static str,
    pub color_label: &'static str,
    pub label_red: &'static str,
    pub label_yellow: &'static str,
    pub label_green: &'static str,
    pub label_blue: &'static str,
    pub label_purple: &'static str,
    pub auto_tone: &'static str,
    pub auto_tone_selection: &'static str,
    pub black_and_white: &'static str,
    pub score_photos: &'static str,
    pub rotate_left: &'static str,
    pub rotate_right: &'static str,
    pub move_to_trash: &'static str,
    pub group_selected: &'static str,
    pub group_bursts: &'static str,
    pub ungroup: &'static str,
    pub delete_group: &'static str,
    pub view: &'static str,
    pub grid: &'static str,
    pub loupe: &'static str,
    pub develop: &'static str,
    pub crop: &'static str,
    pub compare_stack: &'static str,
    pub info_panel: &'static str,
    pub before_after: &'static str,
    pub toggle_zoom: &'static str,
    pub zoom_in: &'static str,
    pub zoom_out: &'static str,
    pub bigger_text: &'static str,
    pub smaller_text: &'static str,
    pub window: &'static str,
    pub minimize: &'static str,
    pub zoom: &'static str,
    pub help: &'static str,
    pub keyboard_shortcuts: &'static str,
}

impl Strings {
    pub fn section(&self, section: Section) -> &'static str {
        match section {
            Section::WhiteBalance => self.white_balance,
            Section::Tone => self.tone,
            Section::Presence => self.presence,
            Section::Detail => self.detail,
            Section::Optics => self.optics,
            Section::Vignette => self.vignette,
            Section::Grain => self.grain,
            Section::Curve => self.curve,
            Section::ColorMixer => self.color_mixer,
            Section::GrayMix => self.gray_mix,
            Section::ColorGrading => self.color_grading,
        }
    }

    pub fn copy_group(&self, group: crate::develop::copy::CopyGroup) -> &'static str {
        use crate::develop::copy::CopyGroup as G;
        match group {
            G::WhiteBalance => self.white_balance,
            G::Tone => self.tone,
            G::Presence => self.presence,
            G::Curve => self.curve,
            G::Color => self.copy_color,
            G::ColorGrading => self.color_grading,
            G::Detail => self.detail,
            G::Effects => self.effects,
            G::Optics => self.optics,
            G::Crop => self.crop_section,
            G::Masks => self.masks,
            G::TouchUp => self.touch_up,
        }
    }

    pub fn copy_item(&self, item: crate::develop::copy::CopyItem) -> &'static str {
        use crate::develop::copy::CopyItem as I;
        match item {
            I::Temp => self.slider(SliderId::Temp),
            I::Tint => self.slider(SliderId::Tint),
            I::Exposure => self.slider(SliderId::Exposure),
            I::Contrast => self.slider(SliderId::Contrast),
            I::Highlights => self.slider(SliderId::Highlights),
            I::Shadows => self.slider(SliderId::Shadows),
            I::Whites => self.slider(SliderId::Whites),
            I::Blacks => self.slider(SliderId::Blacks),
            I::Texture => self.slider(SliderId::Texture),
            I::Clarity => self.slider(SliderId::Clarity),
            I::Dehaze => self.slider(SliderId::Dehaze),
            I::Vibrance => self.slider(SliderId::Vibrance),
            I::Saturation => self.slider(SliderId::Saturation),
            I::Curve => self.curve,
            I::ColorMixer => self.color_mixer,
            I::GrayMix => self.gray_mix,
            I::ColorGrading => self.color_grading,
            I::Sharpening => self.sharpening,
            I::NoiseReduction => self.denoise,
            I::Vignette => self.vignette,
            I::Grain => self.grain,
            I::RemoveCa => self.remove_ca,
            I::Crop => self.crop_section,
            I::Straighten => self.crop_straighten,
            I::Masks => self.masks,
            I::TouchUp => self.touch_up,
        }
    }

    pub fn band(&self, band: crate::develop::Band) -> &'static str {
        self.bands[band as usize]
    }

    pub fn color_axis(&self, axis: crate::develop::ColorAxis) -> &'static str {
        use crate::develop::ColorAxis;
        match axis {
            ColorAxis::Hue => self.mix_hue,
            ColorAxis::Saturation => self.saturation,
            ColorAxis::Luminance => self.mix_luminance,
        }
    }

    pub fn zone(&self, zone: crate::develop::Zone) -> &'static str {
        use crate::develop::Zone;
        match zone {
            Zone::Shadows => self.shadows,
            Zone::Midtones => self.midtones,
            Zone::Highlights => self.highlights,
            Zone::Global => self.global,
        }
    }

    /// What a detected mask selects, as its label names it.
    pub fn target(&self, target: crate::develop::DetectTarget) -> &'static str {
        match target {
            crate::develop::DetectTarget::Subject => self.subject_mask,
            crate::develop::DetectTarget::Sky => self.sky_mask,
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
            SliderId::Texture => self.texture,
            SliderId::Clarity => self.clarity,
            SliderId::Dehaze => self.dehaze,
            SliderId::Vibrance => self.vibrance,
            SliderId::Saturation => self.saturation,
            SliderId::Denoise => self.denoise,
            SliderId::SharpenAmount => self.sharpening,
            SliderId::SharpenRadius => self.sharpen_radius,
            SliderId::SharpenDetail => self.sharpen_detail,
            SliderId::SharpenMasking => self.sharpen_masking,
            SliderId::VignetteAmount | SliderId::GrainAmount => self.amount,
            SliderId::VignetteMidpoint => self.midpoint,
            SliderId::VignetteRoundness => self.roundness,
            SliderId::VignetteFeather => self.feather,
            SliderId::GrainSize => self.grain_size,
            SliderId::GrainRoughness => self.roughness,
            SliderId::Mix(_, band) | SliderId::Gray(band) => self.band(band),
            SliderId::Grade(_, axis) => self.color_axis(axis),
            SliderId::GradeBlending => self.blending,
            SliderId::GradeBalance => self.balance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_map_to_languages() {
        for (tag, want) in [
            ("zh-Hans-CN", Some(Lang::ZhHans)),
            ("zh-Hant-TW", Some(Lang::ZhHant)),
            ("zh-Hant", Some(Lang::ZhHant)),
            ("zh-Hant-HK", Some(Lang::ZhHant)),
            ("zh-TW", Some(Lang::ZhHant)),
            ("zh-HK", Some(Lang::ZhHant)),
            ("zh-MO", Some(Lang::ZhHant)),
            ("zh_TW.UTF-8", Some(Lang::ZhHant)),
            ("zh-Hans-TW", Some(Lang::ZhHans)),
            ("zh-CN", Some(Lang::ZhHans)),
            ("zh-SG", Some(Lang::ZhHans)),
            ("zh_CN.UTF-8", Some(Lang::ZhHans)),
            ("zh", Some(Lang::ZhHans)),
            ("ZH", Some(Lang::ZhHans)),
            ("ja-JP", Some(Lang::Ja)),
            ("ja_JP.UTF-8", Some(Lang::Ja)),
            ("ko-KR", Some(Lang::Ko)),
            ("ko_KR.eucKR", Some(Lang::Ko)),
            ("de-DE", Some(Lang::De)),
            ("de_AT.UTF-8", Some(Lang::De)),
            ("fr-CA", Some(Lang::Fr)),
            ("fr_FR.UTF-8", Some(Lang::Fr)),
            ("en-US", Some(Lang::En)),
            ("en_GB.UTF-8", Some(Lang::En)),
            ("es-ES", None),
            ("C", None),
            ("", None),
        ] {
            assert_eq!(Lang::from_tag(tag), want, "{tag:?}");
        }
    }

    #[test]
    fn stored_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::from_tag(lang.code()), Some(lang));
        }
    }

    /// An install that saved the old single Chinese code keeps Simplified.
    #[test]
    fn the_old_chinese_code_still_loads() {
        assert_eq!(Lang::from_tag("zh"), Some(Lang::ZhHans));
    }

    #[test]
    fn the_url_names_the_language() {
        for (search, want) in [
            ("?lang=de", Some(Lang::De)),
            ("?lang=zh-TW", Some(Lang::ZhHant)),
            ("?lang=zh-CN", Some(Lang::ZhHans)),
            ("?lang=en", Some(Lang::En)),
            ("?a=1&lang=ja&b=2", Some(Lang::Ja)),
            ("lang=fr", Some(Lang::Fr)),
            ("?lang=es", None),
            ("?lang=", None),
            ("?language=de", None),
            ("?xlang=de", None),
            ("?", None),
            ("", None),
        ] {
            assert_eq!(lang_from_query(search), want, "{search:?}");
        }
    }

    #[test]
    fn language_index_matches_the_all_table() {
        for (i, lang) in Lang::ALL.into_iter().enumerate() {
            assert_eq!(lang as usize, i);
        }
    }

    fn catalog(lang: Lang) -> &'static Strings {
        match lang {
            Lang::En => &EN,
            Lang::ZhHans => &ZH_HANS,
            Lang::ZhHant => &ZH_HANT,
            Lang::Ja => &JA,
            Lang::Ko => &KO,
            Lang::De => &DE,
            Lang::Fr => &FR,
        }
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
            (ZH_HANS.capture_date)(2026, 7, 14, 15, 42),
            "2026年7月14日 15:42"
        );
        assert_eq!(
            (ZH_HANS.capture_date)(2026, 1, 1, 0, 5),
            "2026年1月1日 00:05"
        );
        for (lang, want) in [
            (Lang::ZhHant, ["2026年7月14日 15:42", "2026年1月1日 00:05"]),
            (Lang::Ja, ["2026年7月14日 15:42", "2026年1月1日 00:05"]),
            (Lang::Ko, ["2026년 7월 14일 15:42", "2026년 1월 1일 00:05"]),
            (Lang::De, ["14.07.2026 15:42", "01.01.2026 00:05"]),
            (Lang::Fr, ["14/07/2026 15:42", "01/01/2026 00:05"]),
        ] {
            let date = catalog(lang).capture_date;
            assert_eq!(date(2026, 7, 14, 15, 42), want[0], "{lang:?}");
            assert_eq!(date(2026, 1, 1, 0, 5), want[1], "{lang:?}");
        }
    }

    /// Same rule as `scripts/subset-cjk-font.sh`: Han, kana, CJK punctuation
    /// and full-width forms, which Noto Sans SC draws.
    fn is_han_or_kana(c: char) -> bool {
        !is_hangul(c) && matches!(c as u32, 0x2E80..=0x9FFF | 0xFF00..=0xFFEF)
    }

    /// Hangul, which only Noto Sans KR draws.
    fn is_hangul(c: char) -> bool {
        matches!(c as u32, 0x1100..=0x11FF | 0xAC00..=0xD7AF)
    }

    /// Outside macOS the UI's CJK text can come only from the bundled subsets
    /// (no system font that has it, or the web before the full font arrives),
    /// so a character they lack renders as a box there.
    #[test]
    fn bundled_fonts_cover_every_cjk_character() {
        use skrifa::MetadataProvider as _;
        let sc = skrifa::FontRef::new(include_bytes!("../assets/fonts/NotoSansSC-ui-subset.otf"))
            .unwrap();
        let kr = skrifa::FontRef::new(include_bytes!("../assets/fonts/NotoSansKR-ui-subset.otf"))
            .unwrap();
        let (sc, kr) = (sc.charmap(), kr.charmap());
        let catalogs = [
            include_str!("i18n.rs"),
            include_str!("i18n/en.rs"),
            include_str!("i18n/zh_hans.rs"),
            include_str!("i18n/zh_hant.rs"),
            include_str!("i18n/ja.rs"),
            include_str!("i18n/ko.rs"),
            include_str!("i18n/de.rs"),
            include_str!("i18n/fr.rs"),
        ];
        let missing: String = catalogs
            .iter()
            .flat_map(|src| src.chars())
            .filter(|&c| {
                (is_han_or_kana(c) && sc.map(c).is_none()) || (is_hangul(c) && kr.map(c).is_none())
            })
            .collect();
        assert!(
            missing.is_empty(),
            "run scripts/subset-cjk-font.sh; the bundled fonts lack {missing:?}"
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
            "shell/dialog.rs",
            "ui/mod.rs",
            "ui/develop_panel.rs",
            "ui/grid.rs",
            "ui/info_panel.rs",
            "ui/loupe.rs",
            "ui/modals.rs",
            "ui/presets.rs",
            "ui/toolbar.rs",
            "app/mod.rs",
            "app/accessors.rs",
            "app/adjust.rs",
            "app/autotone.rs",
            "app/folder_store.rs",
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

    #[test]
    fn keys_names_the_command_key_this_keyboard_has() {
        let shown = keys("Cmd+[ / Cmd+]");
        if command_is_cmd() {
            assert_eq!(shown, "Cmd+[ / Cmd+]");
        } else {
            assert_eq!(shown, "Ctrl+[ / Ctrl+]");
        }
        assert_eq!(keys("Alt+="), "Alt+=");
    }

    /// Whether the help's key column `keys` names `key` as whole tokens, so
    /// "E" doesn't match inside "Esc".
    fn names_key(keys: &str, key: &str) -> bool {
        let tokens: Vec<&str> = keys.split([' ', '/']).filter(|t| !t.is_empty()).collect();
        let want: Vec<&str> = key.split(' ').collect();
        tokens.windows(want.len()).any(|w| w == want.as_slice())
    }

    /// Every key `App::handle_key` binds, as the overlay writes it. A binding
    /// behind a `SHOW_*` flag is listed only while the flag is on.
    #[test]
    fn help_lists_every_key_binding() {
        let mut bound = vec![
            "Enter",
            "Space",
            "E",
            "G",
            "Esc",
            "I",
            "?",
            "F6",
            "Tab",
            "Page Up",
            "Page Down",
            "0",
            "1",
            "2",
            "3",
            "4",
            "5",
            "Shift+0-5",
            "6",
            "7",
            "8",
            "9",
            "P",
            "X",
            "U",
            "`",
            "D",
            "R",
            "C",
            "Y",
            "Z",
            "[",
            "]",
            "Delete",
            "K",
            "Q",
            "Alt+=",
            "Alt+-",
            "Cmd+O",
            "Cmd+,",
            "Cmd+A",
            "Cmd+D",
            "Cmd+=",
            "Cmd+-",
            "Cmd+[",
            "Cmd+]",
            "Cmd+U",
            "V",
            "Cmd+Shift+C",
            "Cmd+Shift+V",
            "Cmd+Shift+E",
            "Cmd+Z",
            "Cmd+G",
            "Cmd+Shift+G",
            "Shift+Delete",
        ];
        if SUBJECT_KEYS {
            bound.extend(["O", "Shift+O"]);
        }
        let missing: Vec<&str> = bound
            .into_iter()
            .filter(|key| {
                !EN.help
                    .iter()
                    .flat_map(|s| s.rows)
                    .any(|(keys, _)| names_key(keys, key))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "the help overlay leaves out {missing:?}"
        );
        assert!(!names_key("Esc", "E"));
    }

    /// A count in a message must survive translation, in the plural case.
    #[test]
    fn count_messages_show_their_numbers() {
        for lang in Lang::ALL {
            let t = catalog(lang);
            for f in [
                t.selected_n_photos,
                t.n_selected,
                t.selected_n_photos_title,
                t.confirm_apply_settings,
                t.confirm_auto_tone,
                t.confirm_delete,
                t.trash_group_photos,
                t.presets_skipped,
                t.preset_default_name,
                t.deleted,
                t.applied_settings,
            ] {
                assert!(f(7342).contains("7342"), "{lang:?}: {}", f(7342));
            }
            for f in [
                t.tour_step_of,
                t.n_of_m_photos,
                t.bursts_grouped,
                t.confirm_delete_groups,
                t.delete_group_prompt,
                t.deleting,
            ] {
                let text = f(7342, 5821);
                assert!(
                    text.contains("7342") && text.contains("5821"),
                    "{lang:?}: {text}"
                );
            }
        }
    }

    /// Every language lists the same shortcuts in the same order, so the
    /// overlay can't drift between them.
    #[test]
    fn help_tables_have_the_same_shape() {
        for lang in Lang::ALL {
            let tr = catalog(lang);
            assert_eq!(EN.help.len(), tr.help.len(), "{lang:?}");
            for (en, tr_section) in EN.help.iter().zip(tr.help) {
                assert_eq!(
                    en.rows.len(),
                    tr_section.rows.len(),
                    "{lang:?} section {:?}",
                    en.title
                );
            }
        }
    }
}
