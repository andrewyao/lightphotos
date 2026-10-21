// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where the Immich API key is kept. macOS files it in the login Keychain
//! under one account per server. Elsewhere it goes in a file under the config
//! directory that only the user can read, and the export form says so. The
//! file holds one key, with the server it is for on the line above it.

#[cfg(target_os = "macos")]
const SERVICE: &str = "app.lightphotos.immich";

#[cfg(target_os = "macos")]
pub fn load_api_key(server: &str) -> Option<String> {
    let bytes = security_framework::passwords::get_generic_password(SERVICE, server).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(target_os = "macos")]
pub fn save_api_key(server: &str, key: &str) -> Result<(), String> {
    security_framework::passwords::set_generic_password(SERVICE, server, key.as_bytes())
        .map_err(|e| format!("Keychain: {e}"))
}

#[cfg(target_os = "macos")]
pub fn delete_api_key(server: &str) {
    let _ = security_framework::passwords::delete_generic_password(SERVICE, server);
}

#[cfg(not(target_os = "macos"))]
const KEY_FILE: &str = "immich_api_key";

/// One key at a time off macOS, used only for the server it was saved with.
#[cfg(not(target_os = "macos"))]
pub fn load_api_key(server: &str) -> Option<String> {
    key_for(&crate::persist::prefs::load(KEY_FILE)?, server)
}

#[cfg(not(target_os = "macos"))]
pub fn save_api_key(server: &str, key: &str) -> Result<(), String> {
    let dir =
        crate::persist::prefs::config_dir().ok_or_else(|| "no config directory".to_string())?;
    let path = dir.join(KEY_FILE);
    std::fs::create_dir_all(&dir)
        .and_then(|()| write_private(&path, key_file_body(server, key).as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The key file's contents: `server`, then `key`. A normalized server origin
/// has no line break, so the first one ends it.
#[cfg(any(test, not(target_os = "macos")))]
fn key_file_body(server: &str, key: &str) -> String {
    format!("{server}\n{key}")
}

/// The key in `body`, when it was saved for `server`. The server URL and
/// the key are saved one after the other, so a failure between them must not
/// hand one server's key to another. A file from before the server was
/// stored has no line break and gives no key, so the user enters it again
/// once.
#[cfg(any(test, not(target_os = "macos")))]
fn key_for(body: &str, server: &str) -> Option<String> {
    let (saved_for, key) = body.split_once('\n')?;
    (saved_for == server && !key.is_empty()).then(|| key.to_string())
}

/// Write `bytes` to `path`, readable by the user alone.
#[cfg(any(test, not(target_os = "macos")))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    // Created private rather than chmod'ed after, so a new key is never
    // readable.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(path)?;
    // `mode` only applies to a file it creates. One left readable by an
    // older version or another program is narrowed, while still empty,
    // before the key goes in.
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(not(target_os = "macos"))]
pub fn delete_api_key(_server: &str) {
    if let Some(dir) = crate::persist::prefs::config_dir() {
        let _ = std::fs::remove_file(dir.join(KEY_FILE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_key_is_only_for_its_own_server() {
        let body = key_file_body("https://a.example", "secret");
        assert_eq!(
            key_for(&body, "https://a.example").as_deref(),
            Some("secret")
        );
        assert_eq!(key_for(&body, "https://b.example"), None);
    }

    #[test]
    fn a_key_file_with_no_server_gives_no_key() {
        assert_eq!(key_for("secret", "https://a.example"), None);
        assert_eq!(key_for("", "https://a.example"), None);
        assert_eq!(key_for("https://a.example\n", "https://a.example"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_written_over_a_readable_file_leaves_it_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lightphotos-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
