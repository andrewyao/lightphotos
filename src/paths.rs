// SPDX-License-Identifier: GPL-3.0-or-later

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

/// The first free `<stem>.jpg`, `<stem>-1.jpg`, ... name for `src`, skipping
/// names in `existing` (already in `Exports/`) or `taken` (handed out earlier in
/// this batch). The wasm32 version of [`jpg_export_target`], since the browser
/// has no `Path::exists()`.
#[cfg(any(target_arch = "wasm32", test))]
pub fn jpg_export_name(src: &Path, existing: &HashSet<String>, taken: &HashSet<String>) -> String {
    let stem = export_stem(src);
    let free = |name: &str| !existing.contains(name) && !taken.contains(name);

    let base = format!("{stem}.jpg");
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem}-{n}.jpg");
        if free(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The first free `stem.jpg`, `stem-1.jpg`, ... path in `dest_dir` for `src`.
/// Skips files on disk and paths in `taken` (handed out earlier in this batch,
/// not yet written), so same-stem sources like `photo.raw` and `photo.jpg`
/// don't collide.
#[cfg(not(target_arch = "wasm32"))]
pub fn jpg_export_target(src: &Path, dest_dir: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = export_stem(src);

    let free = |candidate: &Path| !candidate.exists() && !taken.contains(candidate);

    let base = dest_dir.join(format!("{stem}.jpg"));
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = dest_dir.join(format!("{stem}-{n}.jpg"));
        if free(&candidate) {
            return candidate;
        }
        n += 1;
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

    #[test]
    fn export_name_avoids_existing_and_taken() {
        let src = Path::new("/photos/DSC_1000.NEF");
        let mut existing = HashSet::new();
        let taken = HashSet::new();
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000.jpg");

        existing.insert("DSC_1000.jpg".to_string());
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000-1.jpg");

        let mut taken = HashSet::new();
        taken.insert("DSC_1000-1.jpg".to_string());
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000-2.jpg");
    }

    #[test]
    fn export_name_falls_back_when_stemless() {
        assert_eq!(
            jpg_export_name(Path::new("/x/.."), &HashSet::new(), &HashSet::new()),
            "export.jpg"
        );
    }

    #[test]
    fn export_target_avoids_clobbering_existing_files() {
        let dir = std::env::temp_dir().join(format!("iv-paths-test-{}", std::process::id()));
        let out = dir.join("Exports");
        std::fs::create_dir_all(&out).unwrap();
        let none = HashSet::new();

        let src = dir.join("photo.png");
        assert_eq!(jpg_export_target(&src, &out, &none), out.join("photo.jpg"));

        std::fs::write(out.join("photo.jpg"), b"x").unwrap();
        assert_eq!(
            jpg_export_target(&src, &out, &none),
            out.join("photo-1.jpg")
        );

        // `taken` reserves names that aren't on disk yet.
        let mut taken = HashSet::new();
        let first = jpg_export_target(&src, &out, &taken);
        taken.insert(first.clone());
        let second = jpg_export_target(&src, &out, &taken);
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
