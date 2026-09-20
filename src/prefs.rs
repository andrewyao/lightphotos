// SPDX-License-Identifier: GPL-3.0-or-later

//! Global app settings: state that belongs to the install rather than to a
//! photo folder. Native keeps one file per key under the OS's config
//! directory, the browser one `localStorage` entry per key.
//!
//! Values are raw strings, not typed values, so a caller can tell a value it
//! cannot parse from one that is missing or unreadable. `presets.rs` needs
//! that distinction to avoid overwriting a corrupt but recoverable library.

#[cfg(not(target_arch = "wasm32"))]
use std::ffi::OsStr;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

/// Which platform's config-directory convention to follow. A parameter of
/// [`config_dir_from`] rather than a `cfg!` inside it, so every platform's
/// path is testable from one host.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    Mac,
    Windows,
    /// The XDG base-directory spec, used everywhere else.
    Xdg,
}

#[cfg(not(target_arch = "wasm32"))]
impl Layout {
    const fn host() -> Layout {
        if cfg!(target_os = "macos") {
            Layout::Mac
        } else if cfg!(windows) {
            Layout::Windows
        } else {
            Layout::Xdg
        }
    }
}

/// The directory holding this install's settings files.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        Layout::host(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("APPDATA").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
    )
}

/// The path derivation, with the three environment variables injected, so a
/// test never reads the developer's own environment.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn config_dir_from(
    layout: Layout,
    home: Option<&OsStr>,
    appdata: Option<&OsStr>,
    xdg: Option<&OsStr>,
) -> Option<PathBuf> {
    Some(match layout {
        Layout::Mac => PathBuf::from(home?).join("Library/Application Support/LightPhotos"),
        Layout::Windows => PathBuf::from(appdata?).join("LightPhotos"),
        Layout::Xdg => xdg
            .map(PathBuf::from)
            .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?
            .join("lightphotos"),
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load(key: &str) -> Option<String> {
    load_at(&config_dir()?.join(key))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save(key: &str, value: &str) -> Result<(), String> {
    let dir = config_dir().ok_or_else(|| "no config directory".to_string())?;
    save_at(&dir.join(key), value)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_at(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn save_at(path: &Path, value: &str) -> Result<(), String> {
    path.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, value))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(target_arch = "wasm32")]
fn storage_key(key: &str) -> String {
    format!("lightphotos.{key}")
}

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

#[cfg(target_arch = "wasm32")]
pub fn load(key: &str) -> Option<String> {
    storage()?.get_item(&storage_key(key)).ok().flatten()
}

/// `set_item` throws `QuotaExceededError` rather than truncating, so the
/// failure has to reach the caller.
#[cfg(target_arch = "wasm32")]
pub fn save(key: &str, value: &str) -> Result<(), String> {
    let storage = storage().ok_or_else(|| "no localStorage".to_string())?;
    storage
        .set_item(&storage_key(key), value)
        .map_err(|e| format!("localStorage: {e:?}"))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_application_support_under_home() {
        assert_eq!(
            config_dir_from(
                Layout::Mac,
                Some(OsStr::new("/Users/x")),
                None,
                Some(OsStr::new("/ignored"))
            ),
            Some(PathBuf::from(
                "/Users/x/Library/Application Support/LightPhotos"
            ))
        );
        assert_eq!(config_dir_from(Layout::Mac, None, None, None), None);
    }

    #[test]
    fn windows_uses_appdata() {
        assert_eq!(
            config_dir_from(
                Layout::Windows,
                Some(OsStr::new("/home/x")),
                Some(OsStr::new("C:\\Users\\x\\AppData\\Roaming")),
                None
            ),
            Some(PathBuf::from("C:\\Users\\x\\AppData\\Roaming").join("LightPhotos"))
        );
        assert_eq!(config_dir_from(Layout::Windows, None, None, None), None);
    }

    #[test]
    fn xdg_config_home_wins_over_the_home_fallback() {
        assert_eq!(
            config_dir_from(
                Layout::Xdg,
                Some(OsStr::new("/home/x")),
                None,
                Some(OsStr::new("/home/x/.conf"))
            ),
            Some(PathBuf::from("/home/x/.conf/lightphotos"))
        );
    }

    #[test]
    fn xdg_falls_back_to_dot_config_under_home() {
        assert_eq!(
            config_dir_from(Layout::Xdg, Some(OsStr::new("/home/x")), None, None),
            Some(PathBuf::from("/home/x/.config/lightphotos"))
        );
        assert_eq!(config_dir_from(Layout::Xdg, None, None, None), None);
    }

    #[test]
    fn save_at_creates_the_directory_and_load_at_reads_it_back() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("language");

        assert_eq!(load_at(&path), None, "nothing stored yet");
        save_at(&path, "zh").unwrap();
        assert_eq!(load_at(&path), Some("zh".to_string()));
        save_at(&path, "en").unwrap();
        assert_eq!(load_at(&path), Some("en".to_string()), "a write replaces");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_at_reports_a_path_it_cannot_write() {
        let dir = std::env::temp_dir().join(format!("lp-prefs-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();

        let err = save_at(&blocker.join("language"), "en").unwrap_err();
        assert!(err.contains("blocker"), "the error names the path: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
