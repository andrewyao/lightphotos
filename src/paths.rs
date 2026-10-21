// SPDX-License-Identifier: MIT OR Apache-2.0

//! Path helpers for file identity and export filenames.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A stable identity for "same file" comparisons and map keys: the canonical
/// path, or the path as given when canonicalization fails (e.g. the file is gone).
pub fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn export_stem(src: &Path) -> String {
    src.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into())
}

/// The first free `<stem>.<ext>`, `<stem>-1.<ext>`, ... name, skipping names
/// in `existing` (already in `Exports/`) or `taken` (handed out earlier in this
/// batch). The wasm32 version of [`export_target`], since the browser has no
/// `Path::exists()`.
#[cfg(any(target_arch = "wasm32", test))]
pub fn export_name(
    stem: &str,
    ext: &str,
    existing: &HashSet<String>,
    taken: &HashSet<String>,
) -> String {
    let free = |name: &str| !existing.contains(name) && !taken.contains(name);
    let base = format!("{stem}.{ext}");
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem}-{n}.{ext}");
        if free(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The first free `<stem>.<ext>`, `<stem>-1.<ext>`, ... path in `dest_dir`.
/// Skips files on disk and paths in `taken` (handed out earlier in this
/// batch, not yet written), so same-stem sources like `photo.raw` and
/// `photo.jpg` don't collide.
#[cfg(not(target_arch = "wasm32"))]
pub fn export_target(stem: &str, ext: &str, dest_dir: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let free = |candidate: &Path| !candidate.exists() && !taken.contains(candidate);
    let base = dest_dir.join(format!("{stem}.{ext}"));
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = dest_dir.join(format!("{stem}-{n}.{ext}"));
        if free(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Why [`read_capped`] got no bytes. Neither names the file: the caller
/// says which file it read.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub enum CappedReadError {
    Io(std::io::Error),
    /// The file is longer than this many bytes.
    TooBig(usize),
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Display for CappedReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CappedReadError::Io(e) => write!(f, "{e}"),
            CappedReadError::TooBig(max) => write!(f, "over the {max}-byte limit"),
        }
    }
}

/// `file`'s bytes, `None` when it is not there. Reads at most `max + 1`
/// bytes, so a huge file costs no more than the cap to refuse, and one that
/// is already over it costs nothing. Anything but a regular file, after
/// symlinks, is refused before it is opened, so a FIFO or a device can't
/// block or stream forever.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_capped(file: &Path, max: usize) -> Result<Option<Vec<u8>>, CappedReadError> {
    use std::io::Read;
    match std::fs::metadata(file) {
        Ok(meta) if !meta.is_file() => {
            return Err(CappedReadError::Io(std::io::Error::other(
                "not a regular file",
            )))
        }
        Ok(meta) if meta.len() > u64::try_from(max).unwrap_or(u64::MAX) => {
            return Err(CappedReadError::TooBig(max))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CappedReadError::Io(e)),
    }
    let f = match std::fs::File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CappedReadError::Io(e)),
    };
    let mut bytes = Vec::new();
    f.take(u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(CappedReadError::Io)?;
    if bytes.len() > max {
        return Err(CappedReadError::TooBig(max));
    }
    Ok(Some(bytes))
}

/// [`write_atomic`], creating `file`'s directory first.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_atomic_in_dir(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = file
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    write_atomic(file, bytes)
}

/// Remove `file`. A file that is already gone counts as removed.
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_if_present(file: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Write `bytes` to `dest` through a sibling temp file and a rename, so a crash
/// never leaves a partial file. The temp file is opened with `create_new`,
/// which refuses any existing name, a symlink included. A folder someone else
/// prepared can plant a link at a temp name, and a plain write would follow it
/// and overwrite whatever it points at.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);

    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let mut name = dest.as_os_str().to_os_string();
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        name.push(format!(".{}-{n}.tmp", std::process::id()));
        let tmp = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 100 => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let written = file.write_all(bytes).and_then(|()| {
        drop(file);
        std::fs::rename(&tmp, dest)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The file a [`write_atomic`] temp name was writing, or `name` itself when it
/// has no `.<pid>-<n>` suffix. `name` has its `.tmp` already stripped.
#[cfg(not(target_arch = "wasm32"))]
pub fn atomic_tmp_target(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((target, tag))
            if tag.split_once('-').is_some_and(|(pid, n)| {
                !pid.is_empty()
                    && !n.is_empty()
                    && pid.bytes().all(|b| b.is_ascii_digit())
                    && n.bytes().all(|b| b.is_ascii_digit())
            }) =>
        {
            target
        }
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_capped_read_refuses_what_is_not_a_regular_file() {
        assert!(read_capped(Path::new("/dev/null"), 16).is_err());
        assert!(read_capped(&std::env::temp_dir(), 16).is_err());
    }

    /// A sparse file claims its length without the disk to back it, so the
    /// cap must hold without reading the file.
    #[test]
    fn a_capped_read_refuses_a_file_over_the_cap() {
        let file = std::env::temp_dir().join(format!("lp-read-capped-{}", std::process::id()));
        std::fs::File::create(&file)
            .and_then(|f| f.set_len(4 << 30))
            .unwrap();
        let read = read_capped(&file, 1 << 20);
        let _ = std::fs::remove_file(&file);
        assert!(matches!(read, Err(CappedReadError::TooBig(_))), "{read:?}");
    }

    #[test]
    fn export_name_avoids_existing_and_taken() {
        let mut existing = HashSet::new();
        let taken = HashSet::new();
        assert_eq!(
            export_name("DSC_1000", "jpg", &existing, &taken),
            "DSC_1000.jpg"
        );

        existing.insert("DSC_1000.jpg".to_string());
        assert_eq!(
            export_name("DSC_1000", "jpg", &existing, &taken),
            "DSC_1000-1.jpg"
        );
        assert_eq!(
            export_name("DSC_1000", "png", &existing, &taken),
            "DSC_1000.png",
            "another format's name is free"
        );

        let mut taken = HashSet::new();
        taken.insert("DSC_1000-1.jpg".to_string());
        assert_eq!(
            export_name("DSC_1000", "jpg", &existing, &taken),
            "DSC_1000-2.jpg"
        );
    }

    #[test]
    fn export_target_avoids_clobbering_existing_files() {
        let dir = std::env::temp_dir().join(format!("iv-paths-test-{}", std::process::id()));
        let out = dir.join("Exports");
        std::fs::create_dir_all(&out).unwrap();
        let none = HashSet::new();

        assert_eq!(
            export_target("photo", "jpg", &out, &none),
            out.join("photo.jpg")
        );

        std::fs::write(out.join("photo.jpg"), b"x").unwrap();
        assert_eq!(
            export_target("photo", "jpg", &out, &none),
            out.join("photo-1.jpg")
        );

        // `taken` reserves names that aren't on disk yet.
        let mut taken = HashSet::new();
        let first = export_target("photo", "jpg", &out, &taken);
        taken.insert(first.clone());
        let second = export_target("photo", "jpg", &out, &taken);
        assert_eq!(first, out.join("photo-1.jpg"));
        assert_eq!(second, out.join("photo-2.jpg"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_atomic_never_writes_through_a_planted_symlink() {
        let dir = std::env::temp_dir().join(format!("lp-atomic-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let dest = dir.join("IMG_1.JPG.xmp");

        let mut planted = vec![dir.join("IMG_1.JPG.xmp.tmp")];
        planted.extend(
            (0..64).map(|n| dir.join(format!("IMG_1.JPG.xmp.{}-{n}.tmp", std::process::id()))),
        );
        for link in &planted {
            std::os::unix::fs::symlink(&victim, link).unwrap();
        }

        write_atomic(&dest, b"{}").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert_eq!(std::fs::read(&dest).unwrap(), b"{}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn atomic_tmp_target_strips_only_the_write_atomic_tag() {
        assert_eq!(atomic_tmp_target("a.thumb.jpg.123-4"), "a.thumb.jpg");
        assert_eq!(atomic_tmp_target("a.thumb.jpg"), "a.thumb.jpg");
        assert_eq!(atomic_tmp_target("a.thumb.jpg.12-"), "a.thumb.jpg.12-");
        assert_eq!(atomic_tmp_target("a.thumb.jpg.x-4"), "a.thumb.jpg.x-4");
    }
}
