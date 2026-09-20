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

/// One group of rows in the keyboard-shortcut overlay: (keys, description).
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
    /// Label of the button that switches to the other language, written in
    /// that language so a reader of either can find it.
    pub other_language: &'static str,
    pub other_language_tip: &'static str,
    pub other: Lang,

    // Header and landing page.
    pub open_folder: &'static str,
    pub open_folder_tip: &'static str,
    pub help_tip: &'static str,
    pub landing_prompt: &'static str,
    pub opening: &'static str,
    pub choose_folder: &'static str,
    pub picker_title: &'static str,

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
    pub settings_from: fn(&str) -> String,
    pub export_jpg: &'static str,
    pub export_jpg_tip: &'static str,
    pub delete: &'static str,
    pub delete_selection_tip: &'static str,

    // Modals.
    pub confirm: &'static str,
    pub cancel: &'static str,
    pub quit_title: &'static str,
    pub quit: &'static str,
    pub shortcuts_title: &'static str,
    pub close: &'static str,
    pub help: &'static [HelpSection],

    // Bulk confirmations.
    pub confirm_clear_rating: fn(usize) -> String,
    pub confirm_rate: fn(&str, usize) -> String,
    pub confirm_export: fn(usize) -> String,
    pub confirm_apply_settings: fn(usize) -> String,
    pub confirm_auto_tone: fn(usize) -> String,
    pub confirm_delete: fn(usize) -> String,

    // Develop panel.
    pub develop: &'static str,
    pub reset: &'static str,
    pub auto: &'static str,
    pub auto_tone_tip: &'static str,
    pub touch_up: &'static str,
    pub brush_size: &'static str,
    pub undo: &'static str,
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
    pub deleted: fn(usize) -> String,
    pub deleted_partial: fn(usize, usize, &str) -> String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub delete_no_handle: fn(&str) -> String,
    pub copied_settings_from: fn(&str) -> String,
    pub applied_settings: fn(usize) -> String,
    pub saved_preset: fn(&str) -> String,
    /// (preset name, photo count)
    pub applied_preset: fn(&str, usize) -> String,
    pub deleted_preset: fn(&str) -> String,
    pub cleared_rating: fn(usize) -> String,
    pub rated: fn(usize, u8) -> String,
    pub kept_best: fn(usize) -> String,
    pub export_no_image: &'static str,
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub presets_locked: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
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
    other_language: "中文",
    other_language_tip: "切换到中文",
    other: Lang::Zh,

    open_folder: "Open\u{2026}",
    open_folder_tip: "Open a different folder (Cmd+O)",
    help_tip: "Keyboard shortcuts (?)",
    landing_prompt: "Choose a folder of photos to get started",
    opening: "Opening\u{2026}",
    choose_folder: "Choose Folder",
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
    settings_from: |name| format!("from {name}"),
    export_jpg: "Export JPG",
    export_jpg_tip: "Export each selected photo as a baked JPG",
    delete: "Delete",
    delete_selection_tip: if WEB {
        "Permanently delete selected photos; cannot be undone (Delete)"
    } else {
        "Move selected photos to the Trash (Delete)"
    },

    confirm: "Confirm",
    cancel: "Cancel",
    quit_title: "Quit LightPhotos?",
    quit: "Quit",
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
                ("E / G", "Editor / library"),
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
            title: "Rate and label",
            rows: &[
                ("0 1 2 3 4 5", "Set star rating 0\u{2013}5"),
                (
                    "Shift+1 \u{2013} 5",
                    "Set color label (red / yellow / green / blue / purple)",
                ),
                ("Shift+0", "Clear color label"),
            ],
        },
        HelpSection {
            title: "Zoom and pan (in editor)",
            rows: &[
                (
                    "Space",
                    "Cycle zoom: fit \u{2192} 2\u{d7} fit \u{2192} 100%",
                ),
                (primary!("+0", " or ", "+)"), "Fit to window"),
                (primary!("+1", " or ", "+!"), "100% (1:1 pixel)"),
                (primary!("++", " or ", "+="), "Zoom in (20% step)"),
                (primary!("+\u{2212}"), "Zoom out (20% step)"),
                ("\u{2191} / \u{2193}", "Zoom in / out (10% step)"),
                ("Shift+Scroll", "Pan horizontally"),
                ("Alt+Scroll", "Pan vertically"),
                ("Shift+Alt+Scroll", "Trackpad zoom"),
                ("Space+Drag", "Pan"),
            ],
        },
        HelpSection {
            title: "Edit",
            rows: &[
                ("[", "Rotate image \u{2212}90\u{b0}"),
                ("]", "Rotate image +90\u{b0}"),
                ("C", "Crop"),
                ("Y", "Before / after compare"),
                (primary!("+U"), "Auto Tone this photo"),
                (primary!("+Shift+U"), "Auto Tone the selection"),
                (primary!("+Shift+C"), "Copy develop settings"),
                (primary!("+Shift+Y"), "Apply settings to selection"),
                ("X", "Export selected as JPG"),
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
            title: "Culling",
            rows: &[
                ("B", "Best-of-burst badges (library)"),
                ("D", "Duplicate-group badges (library)"),
                (
                    "Click badge",
                    "Survey the group: \u{2190}/\u{2192} pick, Enter keeps best, Esc closes",
                ),
            ],
        },
        HelpSection {
            title: "Keyboard focus",
            rows: &[
                ("F6 / Shift+F6", "Cycle focus between regions"),
                (
                    "Tab / Shift+Tab",
                    "Next / previous item in the focused region",
                ),
                ("?", "Show or hide this help"),
            ],
        },
    ],

    confirm_clear_rating: |n| format!("Clear the rating on {n} photo(s)?"),
    confirm_rate: |stars, n| format!("Apply {stars} to {n} photo(s)?"),
    confirm_export: |n| format!("Export {n} photo(s) as JPG?"),
    confirm_apply_settings: |n| format!("Apply the copied settings to {n} photo(s)?"),
    confirm_auto_tone: |n| format!("Auto Tone {n} photo(s)?"),
    confirm_delete: if WEB {
        |n| format!("Permanently delete {n} photo(s)? This cannot be undone.")
    } else {
        |n| format!("Move {n} photo(s) to the Trash?")
    },

    develop: "Develop",
    reset: "Reset",
    auto: "Auto",
    auto_tone_tip: "Set the tone sliders from this photo's own histogram",
    touch_up: "Touch Up",
    brush_size: "Size",
    undo: "Undo",
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
    delete_no_handle: |dir| format!("Could not delete photos: no directory handle for {dir}"),
    copied_settings_from: |name| format!("Copied settings from {name}"),
    applied_settings: |n| format!("Applied settings to {n} photo(s)"),
    saved_preset: |name| format!("Saved preset {name}"),
    applied_preset: |name, n| format!("Applied {name} to {n} photo(s)"),
    deleted_preset: |name| format!("Deleted preset {name}"),
    cleared_rating: |n| format!("Cleared rating on {n} photo(s)"),
    rated: |n, stars| format!("Rated {n} photo(s) \u{2605}{stars}"),
    kept_best: |n| format!("Kept best, rated {n} sibling(s) \u{2605}1"),
    export_no_image: "Export: no image selected",
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
    other_language: "English",
    other_language_tip: "Switch to English",
    other: Lang::En,

    open_folder: "打开\u{2026}",
    open_folder_tip: "打开其他文件夹 (Cmd+O)",
    help_tip: "键盘快捷键 (?)",
    landing_prompt: "选择一个照片文件夹开始",
    opening: "正在打开\u{2026}",
    choose_folder: "选择文件夹",
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
    settings_from: |name| format!("来自 {name}"),
    export_jpg: "导出 JPG",
    export_jpg_tip: "将每张所选照片连同调整导出为 JPG",
    delete: "删除",
    delete_selection_tip: if WEB {
        "永久删除所选照片，无法撤销 (Delete)"
    } else {
        "将所选照片移到废纸篓 (Delete)"
    },

    confirm: "确认",
    cancel: "取消",
    quit_title: "退出 LightPhotos？",
    quit: "退出",
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
                ("E / G", "编辑器 / 图库"),
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
            title: "评分和标签",
            rows: &[
                ("0 1 2 3 4 5", "设置星级 0\u{2013}5"),
                (
                    "Shift+1 \u{2013} 5",
                    "设置颜色标签（红 / 黄 / 绿 / 蓝 / 紫）",
                ),
                ("Shift+0", "清除颜色标签"),
            ],
        },
        HelpSection {
            title: "缩放和平移（编辑器中）",
            rows: &[
                (
                    "Space",
                    "循环缩放：适合 \u{2192} 2\u{d7} 适合 \u{2192} 100%",
                ),
                (primary!("+0", " 或 ", "+)"), "适合窗口"),
                (primary!("+1", " 或 ", "+!"), "100%（1:1 像素）"),
                (primary!("++", " 或 ", "+="), "放大（每次 20%）"),
                (primary!("+\u{2212}"), "缩小（每次 20%）"),
                ("\u{2191} / \u{2193}", "放大 / 缩小（每次 10%）"),
                ("Shift+滚动", "水平平移"),
                ("Alt+滚动", "垂直平移"),
                ("Shift+Alt+滚动", "触控板缩放"),
                ("Space+拖移", "平移"),
            ],
        },
        HelpSection {
            title: "编辑",
            rows: &[
                ("[", "向左旋转 90\u{b0}"),
                ("]", "向右旋转 90\u{b0}"),
                ("C", "裁剪"),
                ("Y", "调整前 / 调整后对比"),
                (primary!("+U"), "对这张照片应用自动色调"),
                (primary!("+Shift+U"), "对所选照片应用自动色调"),
                (primary!("+Shift+C"), "拷贝调整设置"),
                (primary!("+Shift+Y"), "将设置应用到所选照片"),
                ("X", "将所选照片导出为 JPG"),
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
            title: "筛片",
            rows: &[
                ("B", "连拍最佳标记（图库）"),
                ("D", "重复组标记（图库）"),
                (
                    "点按标记",
                    "比较该组：\u{2190}/\u{2192} 挑选，Enter 保留最佳，Esc 关闭",
                ),
            ],
        },
        HelpSection {
            title: "键盘焦点",
            rows: &[
                ("F6 / Shift+F6", "在各区域之间切换焦点"),
                ("Tab / Shift+Tab", "当前区域的下一项 / 上一项"),
                ("?", "显示或隐藏此帮助"),
            ],
        },
    ],

    confirm_clear_rating: |n| format!("清除 {n} 张照片的评分？"),
    confirm_rate: |stars, n| format!("将 {n} 张照片评为 {stars}？"),
    confirm_export: |n| format!("将 {n} 张照片导出为 JPG？"),
    confirm_apply_settings: |n| format!("将拷贝的设置应用到 {n} 张照片？"),
    confirm_auto_tone: |n| format!("对 {n} 张照片应用自动色调？"),
    confirm_delete: if WEB {
        |n| format!("永久删除 {n} 张照片？此操作无法撤销。")
    } else {
        |n| format!("将 {n} 张照片移到废纸篓？")
    },

    develop: "调整",
    reset: "复位",
    auto: "自动",
    auto_tone_tip: "根据这张照片自身的直方图设置色调滑块",
    touch_up: "修补",
    brush_size: "大小",
    undo: "撤销",
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
    delete_no_handle: |dir| format!("无法删除照片：{dir} 没有目录句柄"),
    copied_settings_from: |name| format!("已从 {name} 拷贝设置"),
    applied_settings: |n| format!("已将设置应用到 {n} 张照片"),
    saved_preset: |name| format!("已保存预设 {name}"),
    applied_preset: |name, n| format!("已将 {name} 应用到 {n} 张照片"),
    deleted_preset: |name| format!("已删除预设 {name}"),
    cleared_rating: |n| format!("已清除 {n} 张照片的评分"),
    rated: |n, stars| format!("已将 {n} 张照片评为 \u{2605}{stars}"),
    kept_best: |n| format!("已保留最佳，其余 {n} 张评为 \u{2605}1"),
    export_no_image: "导出：没有选择图像",
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
    fn each_language_offers_the_other() {
        assert_eq!(EN.other, Lang::Zh);
        assert_eq!(ZH.other, Lang::En);
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

    /// The web, Linux, and Windows builds draw Chinese only from the bundled
    /// subset, so a character it lacks renders as a box there.
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
