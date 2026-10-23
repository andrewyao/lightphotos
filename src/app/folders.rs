//! The folders the user has added: the tops of the folder tree. The list is
//! the open catalog's, remembered across launches so the landing page can
//! list them. On the web the catalog lists folders by the names `web_fs`
//! keeps their handles under, each of which the browser must allow again on
//! a later visit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

/// Those of `roots` that aren't a folder right now. Empty on the web, whose
/// missing roots land with the catalog's record in `poll_catalog_load`.
pub(super) fn missing_among(roots: &[PathBuf]) -> HashSet<PathBuf> {
    if cfg!(target_arch = "wasm32") {
        return HashSet::new();
    }
    roots.iter().filter(|r| !r.is_dir()).cloned().collect()
}

/// `root`, then each folder below it down to `dir`. Just `root` when `dir`
/// isn't inside it.
pub(super) fn folder_chain(root: &Path, dir: &Path) -> Vec<PathBuf> {
    if !dir.starts_with(root) {
        return vec![root.to_path_buf()];
    }
    let mut chain: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|a| a.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    chain
}

impl App {
    /// Add `dir` to the tree's roots, unless a root already holds it. The
    /// catalog saves the list.
    pub(super) fn add_root(&mut self, dir: &Path) {
        self.missing_roots.remove(dir);
        if self.catalog.add_folder(dir) {
            self.missing_roots.retain(|r| !r.starts_with(dir));
        }
    }

    /// Expand the tree from `dir`'s root down to `dir`, listing each folder
    /// on the way, so `dir` shows as a row.
    pub(super) fn reveal_folder(&mut self, dir: &Path) {
        let Some(root) = self
            .catalog
            .folders()
            .iter()
            .find(|r| dir.starts_with(r))
            .cloned()
        else {
            return;
        };
        for folder in folder_chain(&root, dir) {
            self.ensure_subdirs(&folder);
            self.expanded.insert(folder);
        }
    }

    /// Drop `root` from the tree and forget it. Its files and sidecars stay.
    /// When the grid shows a folder in it, show the next root that is there,
    /// or the landing page when none is.
    pub(super) fn remove_root(&mut self, root: &Path) {
        if !self.catalog.remove_folder(root) {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        self.forget_web_root(root);
        self.missing_roots.remove(root);
        self.expanded.retain(|p| !p.starts_with(root));
        self.subdirs.retain(|p, _| !p.starts_with(root));
        let showing = self
            .playlist
            .as_ref()
            .is_some_and(|pl| pl.dir().starts_with(root));
        if showing {
            // The roots passed over weren't there; a drive unplugged since
            // launch isn't in `missing_roots` yet.
            let roots = self.catalog.folders();
            let next = roots.iter().position(|r| self.root_ready(r));
            #[cfg(not(target_arch = "wasm32"))]
            self.missing_roots.extend(
                roots
                    .get(..next.unwrap_or(roots.len()))
                    .unwrap_or(&[])
                    .iter()
                    .cloned(),
            );
            match next.and_then(|i| self.catalog.folders().get(i).cloned()) {
                Some(next) => self.open(next),
                None => self.close_folder(),
            }
        }
        self.request_redraw();
    }

    /// Whether `root` can open without asking: natively, it is there; on
    /// the web, the browser has allowed it on this visit.
    fn root_ready(&self, root: &Path) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            root.is_dir()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.web_root_allowed(root)
        }
    }

    /// Back to the landing page with no folder showing.
    pub(super) fn close_folder(&mut self) {
        self.teardown_loupe_state();
        self.finish_folder_jobs();
        self.forget_folder_decodes();
        // A closed folder is free for a headless MCP to write, once its
        // queued writes land.
        #[cfg(not(target_arch = "wasm32"))]
        self.folder_store.leave_dir();
        self.burst_scan = None;
        self.expanded_stacks.clear();
        self.playlist = None;
        self.recompute_visible();
        self.clear_selection();
        self.folder_sel = None;
        self.folder_anchor = None;
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;

    /// A folder holding `a.jpg` and a `sub` folder holding `b.jpg`.
    fn photos(tag: &str) -> PathBuf {
        let dir = temp_folder(tag);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.jpg"), b"").unwrap();
        std::fs::write(dir.join("sub/b.jpg"), b"").unwrap();
        dir
    }

    fn lock_free(dir: &Path) -> bool {
        use crate::sidecar::{FolderLock, Writer};
        FolderLock::try_acquire(dir, Writer::Mcp).is_ok()
    }

    /// A folder holding one empty photo.
    fn one_photo(tag: &str) -> (PathBuf, PathBuf) {
        let dir = temp_folder(tag);
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, b"").unwrap();
        (dir, photo)
    }

    /// Design rows 4 and 15: the app holds the open folder's lock. Leaving
    /// keeps it until the writes queued for the folder land; closing lets
    /// go the same way. A folder without photos takes no lock.
    #[test]
    fn the_open_folder_is_locked_until_its_writes_land_after_the_app_moves_on() {
        use crate::app::test_support::load_folder;
        let ((a, photo), (b, _)) = (one_photo("lock-a"), one_photo("lock-b"));
        let empty = temp_folder("lock-empty");
        let mut app = App::new(None);
        load_folder(&mut app, &a);
        let held_open = !lock_free(&a);
        app.set_rating_of(photo.clone(), 3);
        // Checked before a frame runs, which lets go once the write lands.
        app.load_playlist(Playlist::from_dir(&b), b.clone());
        let held_draining = !lock_free(&a);
        crate::app::test_support::wait_for_folder_store(&mut app);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        app.folder_store.pump();
        let (a_free, b_held) = (lock_free(&a), !lock_free(&b));
        app.close_folder();
        app.folder_store.pump();
        let b_free = lock_free(&b);
        load_folder(&mut app, &empty);
        let empty_untouched = !empty.join(crate::sidecar::SIDECAR_DIR).exists();
        let rated = crate::sidecar::read_record(&photo).unwrap().rating;
        for d in [&a, &b, &empty] {
            let _ = std::fs::remove_dir_all(d);
        }
        assert!(held_open, "the open folder is locked");
        assert!(held_draining, "A stays locked while its write is queued");
        assert!(a_free, "and lets go once it lands");
        assert_eq!(rated, Some(3));
        assert!(b_held && b_free, "B is held while open, free once closed");
        assert!(empty_untouched, "a folder without photos gets nothing");
    }

    /// Design row 8: every spelling of one folder is one folder: a `.`
    /// part, a trailing separator, a symlink to it, and on a
    /// case-insensitive volume another letter case. Opening it again by any
    /// of them, with the lock still held for the first, keeps the visit
    /// writable on that one lock, and a rating lands.
    #[test]
    fn every_spelling_of_one_open_folder_stays_writable_on_one_lock() {
        use crate::app::test_support::load_folder;
        let (dir, photo) = one_photo("lock-spelling");
        let mut spellings = vec![dir.join("."), dir.join("")];
        #[cfg(unix)]
        {
            let link = dir.with_extension("link");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            spellings.push(link);
        }
        let upper = dir.with_file_name(dir.file_name().unwrap().to_string_lossy().to_uppercase());
        if upper != dir && upper.is_dir() {
            spellings.push(upper);
        }
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        let mut seen = Vec::new();
        for (stars, spelling) in (1u8..).zip(&spellings) {
            load_folder(&mut app, spelling);
            app.folder_store.set(&spelling.join("a.jpg"), stars);
            app.folder_store
                .flush_blocking(std::time::Duration::from_secs(10));
            let rated = crate::sidecar::read_record(&photo).unwrap().rating;
            seen.push((app.folder_store.read_only(), rated, lock_free(&dir)));
        }
        let _ = std::fs::remove_file(dir.with_extension("link"));
        let _ = std::fs::remove_dir_all(&dir);
        for (i, (read_only, rated, free)) in seen.into_iter().enumerate() {
            let spelling = spellings[i].display();
            assert_eq!(read_only, None, "{spelling} opened read-only");
            assert_eq!(rated, u8::try_from(i + 1).ok(), "{spelling}: rating saved");
            assert!(!free, "{spelling}: the folder stays locked");
        }
    }

    /// Design rows 30 and 18 (store side): a second instance on a folder
    /// another LightPhotos holds opens read-only, names it, and refuses an
    /// edit before the cache changes. Reopening once free makes it writable.
    #[test]
    fn a_second_instance_opens_read_only_and_refuses_edits_before_the_cache() {
        use crate::app::test_support::load_folder;
        use crate::sidecar::{FolderLock, Writer};
        let (dir, photo) = one_photo("lock-second");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        let by = app.folder_store.read_only();
        let text = app.folder_read_only_text().unwrap_or_default();
        app.folder_store.set(&photo, 4);
        let cache = app.folder_store.get(&photo);
        let refused = app.folder_store.take_refused();
        drop(other);
        app.folder_store.set(&photo, 5);
        let still = app.folder_store.get(&photo);
        load_folder(&mut app, &dir);
        let reopened = app.folder_store.read_only();
        let _ = std::fs::remove_dir_all(&dir);
        let me = std::process::id();
        assert_eq!(by, Some(crate::persist::folder_store::HeldBy::Mcp(me)));
        assert!(text.contains(&me.to_string()), "{text}");
        assert_eq!(cache, None, "refused before the cache changed");
        assert_eq!(refused, std::slice::from_ref(&photo));
        assert_eq!(still, None, "read-only for the whole visit");
        assert_eq!(reopened, None, "reopening once free makes it writable");
    }

    /// Design rows 7 and 26: a lock busy for a moment with no holder named,
    /// as a get_state probe holds it, doesn't make the folder read-only.
    #[test]
    fn a_probe_at_the_moment_of_open_does_not_make_it_read_only() {
        use crate::app::test_support::load_folder;
        use crate::sidecar::{FolderLock, Writer};
        let (dir, _) = one_photo("lock-probe");
        let probe = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let who = dir
            .join(crate::sidecar::SIDECAR_DIR)
            .join(crate::sidecar::WHO_FILE);
        std::fs::remove_file(&who).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(probe);
        });
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        let _ = release.join();
        let read_only = app.folder_store.read_only();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_only, None);
    }

    /// Design row 10: a relaunch while the old app still quits waits for it
    /// (its line says `app-quitting`) instead of opening read-only.
    #[test]
    fn a_relaunch_waits_for_a_quitting_app() {
        use crate::app::test_support::load_folder;
        use crate::sidecar::{FolderLock, Writer};
        let (dir, _) = one_photo("lock-relaunch");
        let mut old = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        old.mark_quitting();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(old);
        });
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        let _ = release.join();
        let read_only = app.folder_store.read_only();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_only, None);
    }

    /// Design row 17: a folder whose lock can't be made (here
    /// `.lightphotos` is a file) opens read-only with that reason.
    #[test]
    fn a_folder_whose_lock_cant_be_made_opens_read_only() {
        use crate::app::test_support::load_folder;
        use crate::persist::folder_store::HeldBy;
        let (dir, _) = one_photo("lock-unavailable");
        std::fs::write(dir.join(crate::sidecar::SIDECAR_DIR), b"not a folder").unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        let by = app.folder_store.read_only();
        let text = app.folder_read_only_text().unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(by, Some(HeldBy::Unlockable));
        assert!(text.contains("can't be locked"), "{text}");
    }

    /// Design row 22: a read-only visit sweeps nothing: the other program
    /// may be writing the files a sweep would take for orphans.
    #[test]
    fn a_read_only_visit_sweeps_no_derived_files() {
        use crate::app::test_support::load_folder;
        use crate::sidecar::{FolderLock, Writer, SIDECAR_DIR};
        let (dir, _) = one_photo("lock-sweep");
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        let orphan = dir
            .join(SIDECAR_DIR)
            .join("gone.jpg.0123456789abcdef.thumb.jpg");
        std::fs::write(&orphan, b"x").unwrap();
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let kept = orphan.exists();
        drop(other);
        load_folder(&mut app, &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while orphan.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let swept = !orphan.exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(kept, "not swept while read-only");
        assert!(swept, "swept once the folder opens writable");
    }

    /// Design rows 18 and 19: a folder another program holds opens
    /// read-only for the whole visit.
    /// The state names the holder and the line under the folder's name
    /// says so. Every way in to an edit (a key, a UI action, a command, an
    /// undo, a menu item) is turned away before anything changes, with the
    /// same reason, so nothing reaches the folder store to be refused. The
    /// lock coming free doesn't make it writable; opening it again does.
    #[test]
    fn a_folder_held_by_another_program_turns_every_edit_away_for_the_visit() {
        use crate::app::test_support::load_folder;
        use crate::persist::folder_store::HeldBy;
        #[cfg(target_os = "macos")]
        use crate::shell::menu::MenuCommand;
        use crate::sidecar::{FolderLock, Writer};
        let (dir, photo) = one_photo("lock-held");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        app.select_single(0);
        let by = app.folder_store.read_only();
        let banner = app.folder_read_only_text().unwrap_or_default();
        let state = serde_json::to_value(crate::app::state::State::of(&app)).unwrap();

        app.handle_key(winit::keyboard::KeyCode::Digit4);
        let by_key = app.status_text().map(str::to_string);
        // K arms the Brush, which no command stands behind.
        app.handle_key(winit::keyboard::KeyCode::KeyK);
        let armed = app.mask_tool();
        app.apply_ui_actions(vec![crate::ui::UiAction::SetRating(3)]);
        let rated = app.run(crate::command::Command::Rate(crate::command::RateParams {
            stars: crate::command::Stars::try_from(2).unwrap(),
            advance: false,
            photos: crate::command::Photos::Selected,
        }));
        let undone = app.run(crate::command::Command::Undo);
        // The menu bar is macOS's alone.
        #[cfg(target_os = "macos")]
        let menu = [
            MenuCommand::Rate(1),
            MenuCommand::Pick,
            MenuCommand::MoveToTrash,
            MenuCommand::AutoToneSelection,
            MenuCommand::Undo,
        ]
        .map(|m| app.menu_enabled(m));
        #[cfg(not(target_os = "macos"))]
        let menu = [false; 5];
        let reached_store = app.folder_store.take_refused();
        let (cache, mirror) = (
            app.folder_store.get(&photo),
            app.ratings.get(&photo).copied(),
        );

        drop(other);
        app.handle_key(winit::keyboard::KeyCode::Digit5);
        let still = app.folder_store.get(&photo);
        load_folder(&mut app, &dir);
        app.select_single(0);
        let reopened = app.folder_store.read_only();
        app.handle_key(winit::keyboard::KeyCode::Digit2);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        let saved = crate::sidecar::read_record(&photo).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);

        let me = std::process::id();
        assert_eq!(by, Some(HeldBy::Mcp(me)));
        assert!(
            banner.contains("lightphotos-mcp") && banner.contains(&me.to_string()),
            "{banner}"
        );
        assert_eq!(state["read_only"], serde_json::json!(banner));
        assert_eq!(by_key.as_deref(), Some(banner.as_str()), "a key says why");
        assert_eq!(armed, None, "no edit tool arms");
        assert!(
            matches!(&rated, Err(crate::command::CommandError::Failed { reason }) if *reason == banner),
            "{rated:?}"
        );
        assert!(undone.is_err(), "undo is turned away too: {undone:?}");
        assert_eq!(menu, [false; 5], "the menu items that edit are off");
        assert!(reached_store.is_empty(), "nothing reached the folder store");
        assert_eq!((cache, mirror), (None, None), "nothing changed");
        assert_eq!(still, None, "read-only for the whole visit");
        assert_eq!(reopened, None, "opening it again takes the lock");
        assert_eq!(saved, Some(2));
    }

    /// Delete on a selected mask is a tool key, handled before the keys
    /// most edits use; a read-only folder still turns it away.
    #[test]
    fn a_read_only_folder_keeps_a_selected_mask_on_delete() {
        use crate::app::test_support::load_folder;
        use crate::develop::{DetectTarget, Mask};
        use crate::sidecar::{FolderLock, Writer};
        let (dir, photo) = one_photo("lock-mask");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        app.select_single(0);
        app.enter_loupe();
        app.shown = crate::app::Shown::Preview(photo.clone(), 1024, 1024);
        app.develop_open = true;
        app.develop_tab = crate::app::DevelopTab::Masks;
        app.masks
            .insert(photo.clone(), vec![Mask::detected(DetectTarget::Sky)]);
        app.mask_selected = Some(0);
        app.handle_key(winit::keyboard::KeyCode::Delete);
        app.handle_key(winit::keyboard::KeyCode::Backspace);
        let left = app.masks.get(&photo).map_or(0, Vec::len);
        let undo = app.history.can_undo();
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, 1, "the mask stays");
        assert!(!undo, "and no undo step was recorded");
    }

    #[test]
    fn the_catalogs_folders_that_are_not_there_are_marked_missing() {
        let here = temp_folder("roots-here");
        let roots = vec![PathBuf::from("/lp-gone/a"), here.clone()];
        assert_eq!(
            missing_among(&roots),
            HashSet::from([PathBuf::from("/lp-gone/a")])
        );
        assert!(missing_among(&[]).is_empty());
    }

    #[test]
    fn opening_a_second_folder_keeps_the_first_in_the_tree() {
        let (a, b) = (photos("roots-a"), photos("roots-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.open_folder(a.join("sub"));
        app.open(b.clone());

        let mut want = vec![a.clone(), b.clone()];
        want.sort_by_cached_key(|r| r.to_string_lossy().to_lowercase());
        assert_eq!(app.folder_roots(), want.as_slice());
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
        let rows = app.visible_tree();
        for row in [&a, &a.join("sub"), &b, &b.join("sub")] {
            assert!(rows.contains(row), "{} not in the tree", row.display());
        }
    }

    fn one_pixel() -> std::sync::Arc<crate::decode::image_decode::DecodedImage> {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields};
        std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
        }))
    }

    /// A full decode is a whole frame the Grid never shows, and another
    /// folder never shows the old one's previews either.
    #[test]
    fn leaving_the_loupe_drops_fulls_and_a_new_folder_drops_previews() {
        use crate::jobs::loader::Loader;
        use crate::jobs::thumbnail::Origin;
        let (a, b) = (photos("loupe-trim-a"), photos("loupe-trim-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.loader = Some(Loader::queue_only_for_test());
        let photo = a.join("a.jpg");
        let loader = app.loader.as_mut().unwrap();
        loader.insert_full_external(photo.clone(), one_pixel(), Origin::Decoded);
        loader.insert_preview_external(photo.clone(), 2560, one_pixel(), Origin::Decoded);

        app.mode = ViewMode::Loupe;
        app.pump();
        assert!(app.loader.as_ref().unwrap().get_full(&photo).is_some());
        app.shown = crate::app::Shown::Full(photo.clone());
        app.mode = ViewMode::Grid;
        app.pump();
        let loader = app.loader.as_ref().unwrap();
        assert!(loader.get_full(&photo).is_none(), "left the Loupe");
        assert!(loader.get_preview(&photo, 2560).is_some());
        assert!(
            !matches!(app.shown, crate::app::Shown::Full(_)),
            "the full on screen goes too"
        );

        app.open(b);
        let loader = app.loader.as_ref().unwrap();
        assert!(loader.get_preview(&photo, 2560).is_none(), "changed folder");
    }

    /// The hold follows the app every frame: a filter that changes the
    /// neighbors without loading a photo moves it too, and leaving and
    /// coming back to the Loupe opens the full tier again.
    #[test]
    fn the_loupe_hold_follows_the_neighbors_and_the_mode() {
        use crate::jobs::loader::Loader;
        use crate::jobs::thumbnail::Origin;
        let (mut app, dir, paths) = crate::app::test_support::folder_app("loupe-hold", 5);
        app.loader = Some(Loader::queue_only_for_test());
        app.sel = Some(2);
        app.want = Some(paths[2].clone());
        app.mode = ViewMode::Loupe;
        app.pump();
        let held = |app: &App| app.loader.as_ref().unwrap().held().fulls.clone();
        assert_eq!(
            held(&app),
            [paths[2].clone(), paths[1].clone(), paths[3].clone()]
        );

        // Hide photo 3 without loading anything: its neighbor is now 4.
        app.visible = vec![0, 1, 2, 4];
        app.pump();
        assert_eq!(
            held(&app),
            [paths[2].clone(), paths[1].clone(), paths[4].clone()]
        );

        app.mode = ViewMode::Grid;
        app.pump();
        app.mode = ViewMode::Loupe;
        app.pump();
        let loader = app.loader.as_mut().unwrap();
        loader.insert_full_external(paths[2].clone(), one_pixel(), Origin::Decoded);
        assert!(loader.get_full(&paths[2]).is_some(), "the tier reopened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_subfolder_of_a_root_reveals_it() {
        let a = photos("roots-reveal");
        let mut app = App::new(None);
        app.open(a.clone());
        app.expanded.clear();
        app.open(a.join("sub"));
        assert_eq!(app.folder_roots(), std::slice::from_ref(&a));
        assert!(app.expanded.contains(&a));
        assert_eq!(app.folder_sel.as_deref(), Some(a.join("sub").as_path()));
    }

    #[test]
    fn left_arrow_stops_at_every_root() {
        let (a, b) = (photos("roots-stop-a"), photos("roots-stop-b"));
        let mut app = App::new(None);
        app.open(a);
        app.open(b.clone());
        app.expanded.remove(&b);
        app.folder_collapse();
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
    }

    #[test]
    fn removing_the_root_showing_opens_another() {
        let (a, b) = (photos("roots-rm-a"), photos("roots-rm-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.open(b.clone());
        app.remove_root(&b);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&a));
        assert_eq!(app.folder_sel.as_deref(), Some(a.as_path()));
        assert!(
            b.join("a.jpg").exists(),
            "removing a folder keeps its files"
        );
    }

    #[test]
    fn removing_the_last_root_returns_to_the_landing_page() {
        let a = photos("roots-rm-last");
        let mut app = App::new(None);
        app.open(a.clone());
        app.remove_root(&a);
        assert!(app.folder_roots().is_empty());
        assert!(!app.has_playlist());
        assert_eq!(app.folder_sel, None);
        assert!(
            a.join("a.jpg").exists(),
            "removing a folder keeps its files"
        );
    }

    /// The other root's drive was unplugged after launch, so `missing_roots`
    /// doesn't know yet. It must not open as a photo with its parent as a
    /// new root.
    #[test]
    fn removing_the_root_showing_skips_a_root_that_has_gone() {
        let (gone, b) = (photos("roots-rm-gone"), photos("roots-rm-stay"));
        let mut app = App::new(None);
        app.open(gone.clone());
        app.open(b.clone());
        std::fs::remove_dir_all(&gone).unwrap();
        app.remove_root(&b);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&gone));
        assert!(app.is_missing_root(&gone));
        assert!(!app.has_playlist());
        assert_eq!(app.mode, ViewMode::Grid);
    }

    /// A path that is neither a folder nor a file, such as a photo deleted
    /// between Finder's open and ours, opens nothing.
    #[test]
    fn opening_a_path_that_is_gone_changes_nothing() {
        let gone = temp_folder("open-gone").join("x.jpg");
        let mut app = App::new(None);
        app.open(gone);
        assert!(app.folder_roots().is_empty());
        assert!(!app.has_playlist());
        assert_eq!(app.mode, ViewMode::Grid);
    }

    #[test]
    fn a_missing_root_stays_listed_and_does_not_open() {
        let gone = temp_folder("roots-gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let mut app = App::new(None);
        app.catalog.add_folder(&gone);
        app.missing_roots.insert(gone.clone());
        app.open_folder(gone.clone());
        assert!(!app.has_playlist());
        assert!(app.is_missing_root(&gone));
        assert_eq!(app.folder_roots(), &[gone]);
    }

    #[test]
    fn a_root_that_comes_back_opens() {
        let back = photos("roots-back");
        let mut app = App::new(None);
        app.catalog.add_folder(&back);
        app.missing_roots.insert(back.clone());
        app.open_folder(back.clone());
        assert!(app.has_playlist());
        assert!(!app.is_missing_root(&back));
    }

    /// With folders listed but none open, the title names no folder; once
    /// one is open it names that one, not the first in the list.
    #[test]
    fn the_title_names_the_folder_showing_not_the_first_root() {
        let (a, b) = (photos("title-a"), photos("title-b"));
        let mut app = App::new(None);
        app.catalog.add_folder(&a);
        app.catalog.add_folder(&b);
        assert_eq!(app.window_title(), "LightPhotos");
        app.open(b.clone());
        let name = b.file_name().unwrap().to_string_lossy();
        assert_eq!(app.window_title(), format!("LightPhotos - {name}"));
    }

    /// A real click on Add Folder over the Grid's folder list.
    #[test]
    fn add_folder_opens_the_picker() {
        use crate::app::test_support::{click, settled};

        let folder = photos("roots-plus");
        let mut app = App::new(None);
        app.open(folder.clone());
        let painted = settled(&mut app);
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !painted.texts().contains(&crate::i18n::t().open_folder),
            "the Grid's header no longer has Open Folder"
        );
        let add = painted.pos_of(crate::i18n::t().add_folder);
        let row = painted.pos_of(&name);
        assert!(add.y < row.y, "Add Folder sits over the folder list");
        assert!(
            add.x < row.x,
            "Add Folder sits flush left, not centred over the list"
        );
        let (actions, _) = click(&mut app, add);
        assert_eq!(actions, vec![crate::ui::UiAction::PickFolder]);
    }

    /// A real click on the landing page's Folders list, through the whole UI.
    #[test]
    fn the_landing_page_lists_remembered_folders() {
        use crate::app::test_support::{click, settled};

        let (a, b) = (photos("roots-home-a"), photos("roots-home-b"));
        let mut app = App::new(None);
        app.catalog.add_folder(&a);
        app.catalog.add_folder(&b);
        let painted = settled(&mut app);
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(painted.texts().contains(&name(&a).as_str()));
        assert!(
            painted.texts().contains(&crate::i18n::t().add_folder),
            "Add Folder adds another folder"
        );
        assert!(
            !painted.has(crate::i18n::t().open_folder),
            "with folders listed, no home card asks for one"
        );
        let (actions, _) = click(&mut app, painted.pos_of(&name(&b)));
        assert_eq!(actions, vec![crate::ui::UiAction::OpenFolder(b.clone())]);
        app.apply_ui_actions(actions);
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
    }

    /// Review 3.7 #2 and #5 (round 2: the UI thread tries the lock once a
    /// frame, never sleeping): opening returns at once and the visit stays
    /// "opening", every edit control off, while an app that is quitting
    /// holds the lock, for longer than a moment (here 3 s). Then it is
    /// writable.
    #[test]
    fn a_relaunch_waits_off_the_ui_thread_for_a_long_quit() {
        use crate::app::test_support::wait_for_folder_store_within;
        use crate::sidecar::{FolderLock, Writer};
        let (dir, _) = one_photo("lock-long-quit");
        let mut old = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        old.mark_quitting();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(old);
        });
        let mut app = App::new(None);
        let started = std::time::Instant::now();
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let ui_wait = started.elapsed();
        let opening = app.read_only() && app.folder_store.opening();
        app.refuse_edit();
        let refused = app.status_text() == Some(crate::i18n::t().folder_opening);
        wait_for_folder_store_within(&mut app, std::time::Duration::from_secs(20));
        let _ = release.join();
        let read_only = app.folder_store.read_only();
        let writable = !app.read_only();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            ui_wait < std::time::Duration::from_millis(500),
            "UI waited {ui_wait:?}"
        );
        assert!(opening && refused, "no edit while the folder opens");
        assert_eq!(read_only, None, "waited for the quitting app");
        assert!(writable);
    }

    /// Review 3.7 #5: once the folder store's writes are on the disk at
    /// quit, the app lets go of its folders, before the slower flushes.
    #[test]
    fn quit_lets_go_of_the_folder_once_its_writes_land() {
        use crate::app::test_support::load_folder;
        let (dir, photo) = one_photo("lock-quit-release");
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        app.folder_store.set(&photo, 3);
        app.folder_store.begin_quit();
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        let held = !lock_free(&dir);
        app.folder_store.release_locks();
        let free = lock_free(&dir);
        let rated = crate::sidecar::read_record(&photo).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(held, "held until released");
        assert!(free, "released after the flush");
        assert_eq!(rated, Some(3));
    }

    /// Review 3.7 #6: a folder opened read-only stays read-only for derived
    /// files after the user moves on, so coverage or thumbnail work still
    /// running for it writes nothing. Reopening it writable clears that.
    #[test]
    fn a_read_only_folder_left_still_gets_no_derived_files() {
        use crate::app::test_support::load_folder;
        use crate::sidecar::{FolderLock, Writer};
        let (ro, ro_photo) = one_photo("lock-derived-ro");
        let (rw, rw_photo) = one_photo("lock-derived-rw");
        let other = FolderLock::try_acquire(&ro, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &ro);
        load_folder(&mut app, &rw);
        let left_ro = app.coverage.read_only_photo(&ro_photo);
        let open_rw = app.coverage.read_only_photo(&rw_photo);
        drop(other);
        load_folder(&mut app, &ro);
        let reopened = app.coverage.read_only_photo(&ro_photo);
        let _ = std::fs::remove_dir_all(&ro);
        let _ = std::fs::remove_dir_all(&rw);
        assert!(left_ro, "still read-only after moving on");
        assert!(!open_rw);
        assert!(!reopened, "writable once reopened free");
    }

    /// Review 3.7 #1: a read-only visit writes no signal cache, so it never
    /// sweeps or replaces the other program's file.
    #[test]
    fn a_read_only_visit_writes_no_signal_cache() {
        use crate::app::test_support::load_folder;
        use crate::persist::signalcache::Signal;
        use crate::scoring::facequality::FaceQuality;
        use crate::sidecar::{FolderLock, Writer, SIDECAR_DIR};
        let (dir, photo) = one_photo("lock-signals");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut app = App::new(None);
        load_folder(&mut app, &dir);
        while app.poll_signal_load() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let faces = FaceQuality {
            faces: 1,
            min_eye_openness: None,
            frame: None,
        };
        app.signals.record(&photo, Signal::Faces(faces));
        app.signals
            .flush_blocking(std::time::Duration::from_secs(5));
        let (empty, _) = one_photo("lock-signals-next");
        load_folder(&mut app, &empty);
        while app.poll_signal_load() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let written = dir
            .join(SIDECAR_DIR)
            .join(crate::persist::signalcache::CACHE_FILE)
            .exists();
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
        assert!(!written, "a read-only visit wrote the signal cache");
    }

    /// A lock an app that is quitting holds, released by dropping the
    /// handle; and a folder with one photo.
    fn quitting_holder(tag: &str) -> (PathBuf, PathBuf, crate::sidecar::FolderLock) {
        use crate::sidecar::{FolderLock, Writer};
        let (dir, photo) = one_photo(tag);
        let mut old = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        old.mark_quitting();
        (dir, photo, old)
    }

    /// Run frames until the folder's visit is decided.
    fn settle_visit(app: &mut App) {
        crate::app::test_support::wait_for_folder_store_within(
            app,
            std::time::Duration::from_secs(20),
        );
    }

    /// Review 3.7 round 2, findings 1 and 6: every lock is the UI thread's,
    /// so A, B, A in quick succession, with loads still running, leaves A
    /// writable on its lock and B free, and a quit right after an open
    /// leaves nothing held once its load lands.
    #[test]
    fn quick_switches_and_a_quit_leave_no_stray_lock() {
        let ((a, photo), (b, _)) = (one_photo("lock-aba-a"), one_photo("lock-aba-b"));
        let mut app = App::new(None);
        for _ in 0..5 {
            app.load_playlist(Playlist::from_dir(&a), a.clone());
            app.load_playlist(Playlist::from_dir(&b), b.clone());
        }
        app.load_playlist(Playlist::from_dir(&a), a.clone());
        settle_visit(&mut app);
        app.pump_folder_store();
        let a_held_open = !lock_free(&a);
        app.set_rating_of(photo.clone(), 2);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        app.pump_folder_store();
        let (a_held, b_free) = (!lock_free(&a), lock_free(&b));
        let writable = !app.read_only();
        let rated = crate::sidecar::read_record(&photo).unwrap().rating;
        app.load_playlist(Playlist::from_dir(&b), b.clone());
        app.folder_store.begin_quit();
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        app.folder_store.release_locks();
        settle_visit(&mut app);
        let all_free = lock_free(&a) && lock_free(&b);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        assert!(a_held_open, "A is held again for its visit");
        assert!(a_held && b_free && writable);
        assert_eq!(rated, Some(2));
        assert!(all_free, "no load thread took a lock after the quit");
    }

    /// Review 3.7 round 2, finding 4: closing a folder while it is opening
    /// ends the attempt; the lock is never taken after.
    #[test]
    fn closing_a_folder_while_it_opens_takes_no_lock() {
        let (dir, _, old) = quitting_holder("lock-close-opening");
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let opening = app.folder_store.opening();
        app.close_folder();
        drop(old);
        for _ in 0..20 {
            app.pump_folder_store();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let free = lock_free(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(opening);
        assert!(free, "the closed folder's lock was never taken");
        assert!(!app.folder_store.opening());
    }

    /// Review 3.7 round 2, finding 2: while the visit is undecided no
    /// derived file is written into the folder.
    #[test]
    fn an_opening_folder_gets_no_derived_files() {
        let (dir, photo, old) = quitting_holder("lock-derived-opening");
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let quiet = app.coverage.read_only_photo(&photo);
        drop(old);
        settle_visit(&mut app);
        let after = app.coverage.read_only_photo(&photo);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(quiet, "no derived write while opening");
        assert!(!after, "derived writes once it opens writable");
    }

    /// Review 3.7 round 2, finding 3: a job's write while the visit is
    /// undecided is held back, and lands once the folder opens writable.
    #[test]
    fn a_write_while_opening_lands_once_writable() {
        let (dir, photo, old) = quitting_holder("lock-held-back-rw");
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        // The records land; the visit is still undecided.
        while app.poll_folder_store_load() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.folder_store.set(&photo, 4);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(5));
        let while_opening = crate::sidecar::read_record(&photo).unwrap().rating;
        drop(old);
        settle_visit(&mut app);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(5));
        let after = crate::sidecar::read_record(&photo).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(while_opening, None, "held back while opening");
        assert_eq!(after, Some(4), "written once writable");
    }

    /// Review 3.7 round 2, finding 3: a write held back while opening is
    /// refused when the folder turns out read-only (here a holder that names
    /// no one and never lets go), and the cache is put back.
    #[test]
    fn a_write_while_opening_is_refused_when_read_only() {
        use crate::sidecar::{FolderLock, Writer};
        let (dir, photo) = one_photo("lock-held-back-ro");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        std::fs::remove_file(
            dir.join(crate::sidecar::SIDECAR_DIR)
                .join(crate::sidecar::WHO_FILE),
        )
        .unwrap();
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        while app.poll_folder_store_load() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        app.folder_store.set(&photo, 4);
        let cached = app.folder_store.get(&photo);
        // A frame at a time, as the app does, until the wait runs out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.folder_store.opening() && std::time::Instant::now() < deadline {
            app.folder_store.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let read_only = app.folder_store.read_only();
        let refused = app.folder_store.take_refused();
        let put_back = app.folder_store.get(&photo);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(5));
        let on_disk = crate::sidecar::read_record(&photo).unwrap().rating;
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(cached, Some(4), "shown while opening");
        assert_eq!(read_only, Some(crate::persist::folder_store::HeldBy::Other));
        assert_eq!(refused, std::slice::from_ref(&photo));
        assert_eq!(put_back, None, "the cache is put back");
        assert_eq!(on_disk, None, "nothing written");
    }

    /// Review 3.7 round 2, finding 9: Auto Tone while the folder opens
    /// defers as it does while the folder loads; only read-only refuses.
    #[test]
    fn auto_tone_while_opening_defers() {
        let (dir, photo, old) = quitting_holder("lock-autotone");
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let opening = app.folder_store.opening();
        app.auto_tone_batch(
            vec![photo.clone()],
            crate::app::autotone::DeferredAutoToneMode::Replace,
        );
        let deferred = app.autotone.take_deferred();
        drop(old);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(opening);
        assert_eq!(deferred, Some(vec![photo]), "deferred, not dropped");
    }
}
