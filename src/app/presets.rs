use super::*;
use crate::develop::copy::CopySections;
use crate::persist::preset_folders::FolderPreset;
use crate::ui::PresetRef;

/// One preset folder's scan.
#[derive(Debug, PartialEq)]
pub(crate) enum FolderState {
    /// No scan has finished since the folder was added or the app started.
    Scanning,
    Ready {
        presets: Vec<FolderPreset>,
        /// `.xmp` files that were not develop presets or would not read.
        skipped: usize,
    },
    /// The browser holds the folder's handle but withholds read access
    /// until a click asks for it.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    NeedsPermission,
    /// The folder could not be read, with why.
    Unavailable(String),
}

impl FolderState {
    /// A scan's outcome as a folder's state.
    fn from_scan(scan: Result<crate::persist::preset_folders::Scan, String>) -> FolderState {
        match scan {
            Ok(scan) => FolderState::Ready {
                presets: scan.presets,
                skipped: scan.skipped,
            },
            Err(e) => FolderState::Unavailable(e),
        }
    }
}

/// A finished scan of one folder.
pub(crate) type FolderScan = (PathBuf, FolderState);

/// A folder preset's group and name, as `preset.apply` lists a clash.
fn label(p: &FolderPreset) -> String {
    format!("{} / {}", p.group, p.name)
}

impl App {
    /// Opens the name prompt for a new preset, seeded with a suggestion. Both
    /// the panel's `+` and `Cmd+Shift+P` take this route.
    pub(super) fn prompt_save_preset(&mut self) {
        self.preset_name_edit = Some((self.suggested_preset_name(), None));
        self.request_redraw();
    }

    /// Opens the name prompt on an existing preset, seeded with its name.
    pub(super) fn prompt_rename_preset(&mut self, id: u64) {
        let Some(name) = self.presets.get(id).map(|p| p.name.clone()) else {
            return;
        };
        self.preset_name_edit = Some((name, Some(id)));
        self.request_redraw();
    }

    pub(super) fn set_preset_name_text(&mut self, text: String) {
        if let Some((buffer, _)) = self.preset_name_edit.as_mut() {
            *buffer = text;
        }
        self.request_redraw();
    }

    /// A blank name leaves the prompt open, so Enter on an empty field is not a
    /// silent no-op.
    pub(super) fn commit_preset_name(&mut self) {
        let Some((name, target)) = self.preset_name_edit.clone() else {
            return;
        };
        if name.trim().is_empty() {
            return;
        }
        self.preset_name_edit = None;
        match target {
            None => self.save_preset_from_shown(&name),
            Some(id) => self.rename_preset(id, &name),
        }
    }

    pub(super) fn cancel_preset_name(&mut self) {
        self.preset_name_edit = None;
        self.request_redraw();
    }

    fn rename_preset(&mut self, id: u64, name: &str) {
        self.presets.rename(id, name);
        let Some(stored) = self.presets.get(id).map(|p| p.name.clone()) else {
            return;
        };
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().renamed_preset)(&stored),
        );
        self.request_redraw();
    }

    /// The first `Preset N` the library does not already have, so a suggestion
    /// never arrives pre-suffixed.
    fn suggested_preset_name(&self) -> String {
        let default = crate::i18n::t().preset_default_name;
        (1usize..)
            .map(default)
            .find(|name| {
                !self
                    .presets
                    .presets()
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(name))
            })
            .unwrap_or_default()
    }

    /// Saves the shown photo's tone settings under `name`, with no crop,
    /// rotation or touch-ups, so the look can be applied to any other photo.
    fn save_preset_from_shown(&mut self, name: &str) {
        let tone = self.current_adjustments().tone_only();
        let Some(stored) = self.presets.add(name, tone, Vec::new()) else {
            return;
        };
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().saved_preset)(&stored),
        );
        self.request_redraw();
    }

    /// Applies a preset to every photo the keyboard acts on, with no
    /// confirmation: the batch is one undo step.
    pub(super) fn apply_preset(&mut self, preset: &PresetRef) {
        let Some((name, look)) = self.resolve_preset(preset) else {
            return;
        };
        let n = self.apply_look_to(&look, CopySections::LOOK, &self.action_paths());
        if n == 0 {
            return;
        }
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().applied_preset)(&name, n),
        );
    }

    /// A preset's name and look. A folder preset comes from the last scan.
    pub(crate) fn resolve_preset(&self, preset: &PresetRef) -> Option<(String, Adjustments)> {
        match preset {
            PresetRef::Library(id) => self
                .presets
                .get(*id)
                .map(|p| (p.name.clone(), p.adjustments)),
            PresetRef::Folder(path) => self
                .folder_presets()
                .find(|p| &p.path == path)
                .map(|p| (p.name.clone(), p.adj)),
        }
    }

    /// Every preset the last scans found, folder by folder.
    pub(crate) fn folder_presets(&self) -> impl Iterator<Item = &FolderPreset> {
        self.preset_folders
            .iter()
            .flat_map(|(_, state)| match state {
                FolderState::Ready { presets, .. } => presets.as_slice(),
                FolderState::Scanning
                | FolderState::NeedsPermission
                | FolderState::Unavailable(_) => &[],
            })
    }

    /// The preset `preset.apply` names: the library's of that exact name,
    /// else the one folder preset of that name, in `group` when given.
    pub(super) fn find_preset(&self, name: &str, group: Option<&str>) -> Result<PresetRef, String> {
        if group.is_none() {
            if let Some(p) = self.presets.presets().iter().find(|p| p.name == name) {
                return Ok(PresetRef::Library(p.id));
            }
        }
        let found: Vec<&FolderPreset> = self
            .folder_presets()
            .filter(|p| p.name == name && group.is_none_or(|g| p.group == g))
            .collect();
        match found.as_slice() {
            [one] => Ok(PresetRef::Folder(one.path.clone())),
            [] if self.preset_scan.is_some() => {
                Err("the preset folders are still being read; try again once idle".into())
            }
            [] => {
                const SHOWN: usize = 20;
                let names: Vec<String> = self
                    .presets
                    .presets()
                    .iter()
                    .map(|p| p.name.clone())
                    .chain(self.folder_presets().map(label))
                    .collect();
                let more = names.len().saturating_sub(SHOWN);
                let mut list = names.into_iter().take(SHOWN).collect::<Vec<_>>().join(", ");
                if more > 0 {
                    list.push_str(&format!(" and {more} more"));
                }
                Err(format!("no preset named `{name}`; the presets are: {list}"))
            }
            many => {
                let labels: Vec<String> = many.iter().map(|p| label(p)).collect();
                Err(format!(
                    "{} folder presets are named `{name}`: {}; give its `group`",
                    many.len(),
                    labels.join(", ")
                ))
            }
        }
    }

    /// Adds a preset folder to the catalog and scans it. `Err` for a path
    /// that is not a folder. The catalog keeps the absolute path, so a
    /// relative one names the same folder on the next launch.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn add_preset_folder(&mut self, dir: &Path) -> Result<(), String> {
        let dir = std::fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a folder", dir.display()));
        }
        if self.catalog.add_preset_folder(&dir) {
            self.rescan_preset_folders(false);
        }
        Ok(())
    }

    /// The browser adds a preset folder through its picker, which hands over
    /// a handle a path cannot name.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn add_preset_folder(&mut self, _dir: &Path) -> Result<(), String> {
        Err("the browser adds preset folders through Add preset folder…".into())
    }

    /// Add preset folder…: the browser's folder picker. The pick lands in
    /// [`poll_preset_scans`](Self::poll_preset_scans).
    #[cfg(target_arch = "wasm32")]
    pub(super) fn add_preset_folder_dialog(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.preset_pick = Some(rx);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = tx.send(crate::web::web_fs::pick_preset_folder().await);
        });
    }

    /// A finished browser pick: the folder joins the catalog, or a cancel
    /// does nothing.
    #[cfg(target_arch = "wasm32")]
    fn poll_preset_pick(&mut self) -> bool {
        let Some(rx) = &self.preset_pick else {
            return false;
        };
        let picked = match rx.try_recv() {
            Ok(picked) => picked,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.preset_pick = None;
                return false;
            }
        };
        self.preset_pick = None;
        match picked {
            Ok(name) => {
                if self.catalog.add_preset_folder(&name) {
                    self.rescan_preset_folders(false);
                }
            }
            Err(e) => web_sys::console::log_1(&format!("[presets] no folder picked: {e}").into()),
        }
        false
    }

    /// Add preset folder…: the folder picker, then the add.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn add_preset_folder_dialog(&mut self) {
        let Some(dir) = crate::shell::dialog::pick_preset_folder() else {
            return;
        };
        if let Err(e) = self.add_preset_folder(&dir) {
            self.set_status(
                StatusKind::Error,
                (crate::i18n::t().preset_folder_add_failed)(&e),
            );
        }
    }

    pub(super) fn remove_preset_folder(&mut self, dir: &Path) {
        if self.catalog.remove_preset_folder(dir) {
            #[cfg(target_arch = "wasm32")]
            wasm_bindgen_futures::spawn_local(
                crate::web::web_catalog_idb::forget_preset_folder_if_unlisted(
                    self.catalog.name().to_string(),
                    dir.to_path_buf(),
                ),
            );
            self.rescan_preset_folders(false);
        }
    }

    /// Reads the catalog's preset folders again. Each keeps showing its last
    /// result until the new one lands; a folder no longer listed goes. `ask`
    /// is for a click, which lets the browser ask for access to a folder it
    /// withholds; natively it changes nothing.
    pub(crate) fn rescan_preset_folders(&mut self, ask: bool) {
        let folders = self.catalog.preset_folders().to_vec();
        let mut old = std::mem::take(&mut self.preset_folders);
        self.preset_folders = folders
            .iter()
            .map(|f| {
                let state = old
                    .iter()
                    .position(|(o, _)| o == f)
                    .map_or(FolderState::Scanning, |i| old.swap_remove(i).1);
                (f.clone(), state)
            })
            .collect();
        self.preset_scan = None;
        if folders.is_empty() {
            return;
        }
        match spawn_scan(folders, ask) {
            Ok(rx) => self.preset_scan = Some(rx),
            Err(e) => {
                for (_, state) in &mut self.preset_folders {
                    *state = FolderState::Unavailable(e.clone());
                }
            }
        }
        self.request_redraw();
    }

    /// Lands finished scans. True while the latest scan is still running.
    pub(crate) fn poll_preset_scans(&mut self) -> bool {
        #[cfg(target_arch = "wasm32")]
        let picking = self.poll_preset_pick();
        #[cfg(not(target_arch = "wasm32"))]
        let picking = false;
        let Some(rx) = &self.preset_scan else {
            return picking;
        };
        let mut landed = Vec::new();
        let running = loop {
            match rx.try_recv() {
                Ok(scan) => landed.push(scan),
                Err(std::sync::mpsc::TryRecvError::Empty) => break true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break false,
            }
        };
        if !running {
            self.preset_scan = None;
        }
        if landed.is_empty() {
            return running || picking;
        }
        for (folder, scanned) in landed {
            if let Some((_, state)) = self.preset_folders.iter_mut().find(|(f, _)| *f == folder) {
                *state = scanned;
            }
        }
        self.request_redraw();
        running || picking
    }

    /// Opens the Lightroom preset picker and imports what the user chose.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn import_lr_presets(&mut self) {
        let paths = crate::shell::dialog::pick_xmp_files();
        self.add_lr_presets(&paths);
    }

    /// Reads each `.xmp` and stores what it could map, under the preset's own
    /// `crs:Name` or the file stem. The batch is one write.
    #[cfg(not(target_arch = "wasm32"))]
    fn add_lr_presets(&mut self, paths: &[PathBuf]) {
        // Cancelling the picker is not an event worth a toast.
        if paths.is_empty() {
            return;
        }
        let t = crate::i18n::t();
        let mut looks = Vec::new();
        let mut failures = Vec::new();
        for path in paths {
            let imported = std::fs::read_to_string(path)
                .map_err(|e| e.to_string())
                .and_then(|xmp| crate::persist::lr_preset::parse(&xmp));
            let imported = match imported {
                Ok(imported) => imported,
                Err(reason) => {
                    let file = path.file_name().unwrap_or_default().to_string_lossy();
                    failures.push((t.lr_import_failed)(&file, &reason));
                    continue;
                }
            };
            let notes = crate::persist::preset_folders::notes(&imported);
            let name = imported.name.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            });
            looks.push((name, imported.adj, notes));
        }

        let stored = self.presets.add_all(looks);
        // One status line, built once. Two `set_status` calls cannot both
        // survive, because it replaces the message rather than appending. What
        // each preset could not carry lives on the row as hover text, which is
        // the durable channel for it; this line lasts three seconds.
        let kind = if failures.is_empty() {
            StatusKind::Success
        } else {
            StatusKind::Error
        };
        self.set_status(
            kind,
            (t.lr_import_status)(
                stored.len(),
                failures.len(),
                failures.first().map_or("", String::as_str),
            ),
        );
        self.request_redraw();
    }

    pub(super) fn delete_preset(&mut self, id: u64) {
        let Some(name) = self.presets.get(id).map(|p| p.name.clone()) else {
            return;
        };
        self.presets.remove(id);
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().deleted_preset)(&name),
        );
        self.request_redraw();
    }
}

/// Scans `folders` in order on a thread of its own, sending each result as
/// it lands. The thread stops early once the receiver is dropped.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_scan(folders: Vec<PathBuf>, _ask: bool) -> Result<Receiver<FolderScan>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("preset-scan".into())
        .spawn(move || {
            for folder in folders {
                use crate::persist::preset_folders::{scan, LIMITS};
                let state = FolderState::from_scan(scan(&folder, &LIMITS));
                if tx.send((folder, state)).is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

/// The browser's scan: each folder through its kept handle, one after
/// another on the page's own task. A folder the browser withholds access to
/// waits for a click to `ask`.
#[cfg(target_arch = "wasm32")]
fn spawn_scan(folders: Vec<PathBuf>, ask: bool) -> Result<Receiver<FolderScan>, String> {
    use crate::web::web_fs;
    let (tx, rx) = std::sync::mpsc::channel();
    wasm_bindgen_futures::spawn_local(async move {
        for folder in folders {
            let state = match web_fs::reopen_preset_folder(&folder.to_string_lossy(), ask).await {
                Ok(Some(handle)) => {
                    FolderState::from_scan(Ok(web_fs::scan_preset_folder(&folder, handle).await))
                }
                Ok(None) => FolderState::NeedsPermission,
                Err(e) => FolderState::Unavailable(e),
            };
            if tx.send((folder, state)).is_err() {
                return;
            }
        }
    });
    Ok(rx)
}

#[cfg(test)]
pub(in crate::app) mod tests {
    use super::*;
    use crate::app::test_support::*;
    use crate::command::Command;
    use crate::develop::Crop;
    use crate::persist::folder_store::FolderStore;

    const CROP: Crop = Crop {
        left: 0.1,
        top: 0.1,
        right: 0.9,
        bottom: 0.9,
    };

    /// The shape Camera Raw writes: attributes on the tag, the name in an
    /// `rdf:Alt`, and one field this app has no slider for.
    const NAMED_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\" \
        xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
        crs:Exposure2012=\"+0.35\" crs:Contrast2012=\"+15\" crs:CropAngle=\"+22\">\
        <crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">Warm Film</rdf:li></rdf:Alt></crs:Name>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";

    /// A preset with no `crs:Name` of its own, in Lightroom's B&W mode.
    const GRAYSCALE_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\" \
        xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
        crs:Exposure2012=\"-0.2\" crs:ConvertToGrayscale=\"True\"/>\
        </rdf:RDF></x:xmpmeta>";

    /// Sidecar writes are queued behind `FolderStore`'s write-back worker, so a
    /// test that reads the disk waits here first. A timeout rather than an
    /// unbounded wait, so a stuck writer fails the assertion instead of
    /// hanging the suite.
    fn flush(app: &mut App) {
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
    }

    fn tone(exposure: f32, contrast: f32) -> Adjustments {
        Adjustments {
            exposure,
            contrast,
            ..Default::default()
        }
    }

    /// A Lightroom preset as exported, with its group left to the folder.
    fn lr_preset(name: &str, exposure: &str, contrast: &str) -> String {
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" \
             xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
             crs:PresetType=\"Normal\" crs:Exposure2012=\"{exposure}\" \
             crs:Contrast2012=\"{contrast}\" crs:HasSettings=\"True\">\
             <crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">{name}</rdf:li></rdf:Alt></crs:Name>\
             </rdf:Description></rdf:RDF></x:xmpmeta>"
        )
    }

    /// A preset folder holding `files` of `(relative path, xmp)`, by the
    /// absolute path the catalog keeps.
    fn preset_dir(tag: &str, files: &[(&str, String)]) -> PathBuf {
        let dir = std::fs::canonicalize(temp_folder(tag)).unwrap();
        for (rel, xmp) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, xmp).unwrap();
        }
        dir
    }

    fn wait_for_scans(app: &mut App) {
        let start = std::time::Instant::now();
        while app.poll_preset_scans() {
            assert!(
                start.elapsed().as_secs() < 10,
                "the preset scan never ended"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn exposure_of(app: &App, path: &Path) -> f32 {
        app.edits.get(path).map_or(0.0, |a| a.exposure)
    }

    /// Catches an apply that reaches only the shown photo, one that asks
    /// first, and a batch that takes one undo per photo or undoes the edit
    /// before it too.
    #[test]
    fn a_folder_preset_applies_to_the_selection_and_one_undo_reverts_it() {
        let (mut app, dir, paths) = folder_app("folder-apply", 3);
        let presets = preset_dir(
            "folder-apply-presets",
            &[("Film/Warm.xmp", lr_preset("Warm", "+0.5", "+10"))],
        );
        app.selected = [0].into();
        app.edit_each(&paths[..1], |adj| Adjustments {
            exposure: 0.1,
            ..adj
        });
        app.save_edit_unless_dragging();

        app.add_preset_folder(&presets).unwrap();
        wait_for_scans(&mut app);
        let warm = presets.join("Film/Warm.xmp");
        assert_eq!(
            app.resolve_preset(&PresetRef::Folder(warm.clone())),
            Some(("Warm".to_string(), tone(0.5, 10.0)))
        );

        app.selected = (0..3).collect();
        app.apply_ui_actions(vec![crate::ui::UiAction::ApplyPreset(PresetRef::Folder(
            warm,
        ))]);
        app.save_edit_unless_dragging();

        assert!(!app.confirm_open(), "no confirmation");
        for path in &paths {
            assert_eq!(app.edits.get(path), Some(&tone(0.5, 10.0)), "{path:?}");
        }
        assert_eq!(
            app.status_text(),
            Some((crate::i18n::t().applied_preset)("Warm", 3).as_str())
        );

        #[cfg(target_os = "macos")]
        let cmd = ModifiersState::SUPER;
        #[cfg(not(target_os = "macos"))]
        let cmd = ModifiersState::CONTROL;
        app.modifiers = cmd;
        app.handle_key(KeyCode::KeyZ);

        assert_eq!(exposure_of(&app, &paths[0]), 0.1, "back to its own edit");
        assert!(!app.edits.contains_key(&paths[1]));
        assert!(!app.edits.contains_key(&paths[2]));
        flush(&mut app);
        let folder_store = FolderStore::with_dir(dir.clone());
        assert_eq!(folder_store.adjustments(&paths[0]).exposure, 0.1);
        assert!(folder_store.adjustments(&paths[1]).is_identity());
        assert!(folder_store.adjustments(&paths[2]).is_identity());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// In the Loupe with nothing picked in the Grid, the photo on screen is
    /// what a preset changes.
    #[test]
    fn a_loupe_photo_with_no_grid_selection_is_the_target() {
        let (mut app, dir, paths) = folder_app("loupe-target", 3);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        let golden = app.presets.presets()[0].id;
        app.mode = ViewMode::Loupe;
        app.selected.clear();
        app.sel = None;
        app.want = Some(paths[1].clone());

        app.apply_preset(&PresetRef::Library(golden));

        assert_eq!(app.edits.get(&paths[1]), Some(&tone(0.4, 0.0)));
        assert!(!app.edits.contains_key(&paths[0]));
        assert!(!app.edits.contains_key(&paths[2]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches `preset.apply` picking one of two same-named folder presets
    /// by scan luck, and `group` not narrowing it.
    #[test]
    fn a_name_two_folders_hold_needs_its_group() {
        let (mut app, dir, paths) = folder_app("preset-group", 1);
        let presets = preset_dir(
            "preset-group-presets",
            &[
                ("Film/a.xmp", lr_preset("Warm", "+0.5", "0")),
                ("Travel/b.xmp", lr_preset("Warm", "-0.5", "0")),
            ],
        );
        app.run(Command::AddPresetFolder(
            crate::command::PresetFolderParams {
                path: presets.clone(),
            },
        ))
        .unwrap();
        wait_for_scans(&mut app);
        let apply = |group: Option<&str>| {
            Command::Preset(crate::command::PresetParams {
                name: "Warm".into(),
                group: group.map(str::to_string),
                photos: crate::command::Photos::Named(vec![paths[0].clone()]),
            })
        };

        let err = app.run(apply(None)).unwrap_err().to_string();
        assert!(
            err.contains("Film / Warm") && err.contains("Travel / Warm"),
            "the error lists both: {err}"
        );
        assert!(app.edits.is_empty());

        app.run(apply(Some("Travel"))).unwrap();
        assert_eq!(exposure_of(&app, &paths[0]), -0.5);

        app.presets.add("Warm", tone(0.2, 0.0), Vec::new());
        app.run(apply(None)).unwrap();
        assert_eq!(
            exposure_of(&app, &paths[0]),
            0.2,
            "the library's comes first"
        );

        let missing = app.run(Command::AddPresetFolder(
            crate::command::PresetFolderParams {
                path: presets.join("nope"),
            },
        ));
        assert!(missing.is_err(), "a path that is not a folder is refused");
        assert_eq!(app.catalog.preset_folders(), std::slice::from_ref(&presets));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// The catalog keeps the folder itself, so a path through a link, or a
    /// relative one, still names it next launch, and the same folder by two
    /// paths is listed once.
    #[cfg(unix)]
    #[test]
    fn a_preset_folder_is_kept_by_its_real_path() {
        let (mut app, dir, _) = folder_app("preset-real-path", 1);
        let presets = preset_dir("preset-real-path-presets", &[]);
        let link = temp_folder("preset-real-path-link").join("presets");
        std::os::unix::fs::symlink(&presets, &link).unwrap();

        app.add_preset_folder(&link).unwrap();
        app.add_preset_folder(&presets).unwrap();

        assert_eq!(app.catalog.preset_folders(), std::slice::from_ref(&presets));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
        let _ = std::fs::remove_dir_all(link.parent().unwrap());
    }

    /// Catches a switch that keeps the last catalog's folder presets, or one
    /// that never reads the new catalog's.
    #[test]
    fn switching_catalogs_swaps_the_folder_presets() {
        let (mut app, dir, _) = folder_app("preset-switch", 1);
        let presets = preset_dir(
            "preset-switch-presets",
            &[("Warm.xmp", lr_preset("Warm", "+0.5", "0"))],
        );
        let at = crate::persist::catalog::CatalogRef::in_dir(&dir, "trip").unwrap();
        let mut trip = crate::persist::catalog::Catalog::create_at(at.clone(), Vec::new()).unwrap();
        trip.add_preset_folder(&presets);
        trip.flush_blocking(std::time::Duration::from_secs(10));
        drop(trip);

        app.open_catalog(at);
        wait_for_scans(&mut app);
        let names: Vec<&str> = app.folder_presets().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Warm"]);

        app.switch_catalog(crate::persist::catalog::Catalog::in_memory("empty"));
        assert_eq!(app.folder_presets().count(), 0);
        assert!(app.preset_folders.is_empty());

        app.open_catalog(crate::persist::catalog::CatalogRef::in_dir(&dir, "trip").unwrap());
        assert!(app.preset_scan.is_some(), "the trip's scan is under way");
        app.switch_catalog(crate::persist::catalog::Catalog::in_memory("empty"));
        assert!(
            app.preset_scan.is_none(),
            "a switch drops the scan of the catalog it left"
        );
        wait_for_scans(&mut app);
        assert_eq!(app.folder_presets().count(), 0, "so nothing of it lands");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// Catches a preset list that won't read being reported as a failed
    /// catalog save, which sends the user looking for a disk problem.
    #[test]
    fn unreadable_preset_folders_say_so_not_that_the_catalog_failed_to_save() {
        let (mut app, dir, _) = folder_app("preset-unreadable", 1);
        let at = crate::persist::catalog::CatalogRef::in_dir(&dir, "trip").unwrap();
        drop(crate::persist::catalog::Catalog::create_at(at.clone(), Vec::new()).unwrap());
        std::fs::write(at.path().join("presets.json"), b"{\"v\":1,\"folders\":").unwrap();

        app.open_catalog(at);
        app.show_catalog_errors();

        let t = crate::i18n::t();
        let status = app.status_text().unwrap_or_default().to_string();
        let prefix = (t.preset_folders_unreadable)("");
        assert!(
            status.starts_with(&prefix) && status.contains("presets.json"),
            "says the preset folders didn't read, and which file: {status}"
        );
        assert!(
            !status.starts_with(&(t.catalog_save_failed)("")),
            "not a failed save: {status}"
        );
        assert_eq!(app.catalog.name(), "trip", "the catalog still opened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A slow scan from before a rescan must not land over the newer one.
    /// The stale sender stands in for that scan's thread.
    #[test]
    fn a_scan_superseded_by_a_rescan_never_lands() {
        let (mut app, dir, _) = folder_app("preset-stale", 1);
        let presets = preset_dir(
            "preset-stale-presets",
            &[("New.xmp", lr_preset("New", "+1", "0"))],
        );
        app.catalog.add_preset_folder(&presets);
        let (stale_tx, stale_rx) = std::sync::mpsc::channel();
        app.preset_scan = Some(stale_rx);

        app.rescan_preset_folders(false);
        let stale = FolderState::Ready {
            presets: vec![FolderPreset {
                path: presets.join("Old.xmp"),
                group: "Old".into(),
                name: "Old".into(),
                adj: tone(-1.0, 0.0),
                notes: Vec::new(),
            }],
            skipped: 0,
        };
        let _ = stale_tx.send((presets.clone(), stale));
        wait_for_scans(&mut app);

        let names: Vec<&str> = app.folder_presets().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["New"]);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    #[test]
    fn a_saved_preset_keeps_the_tone_and_drops_the_crop() {
        let (mut app, dir, paths) = folder_app("save", 1);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.apply_adjustments(Adjustments {
            crop: Some(CROP),
            ..tone(0.7, 20.0)
        });

        app.save_preset_from_shown("Golden");

        let saved = &app.presets.presets()[0];
        assert_eq!(saved.name, "Golden");
        assert_eq!(saved.adjustments, tone(0.7, 20.0));
        assert_eq!(
            saved.adjustments.crop, None,
            "a preset never carries the source photo's crop"
        );
        assert_eq!(
            app.edits.get(&paths[0]).unwrap().crop,
            Some(CROP),
            "saving a preset does not disturb the photo it came from"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applying_a_preset_writes_the_selected_photo_and_its_sidecar() {
        let (mut app, dir, paths) = folder_app("shown", 2);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.sel = Some(0);
        let id = {
            app.presets.add("Golden", tone(0.5, 10.0), Vec::new());
            app.presets.presets()[0].id
        };

        app.apply_preset(&PresetRef::Library(id));

        assert_eq!(app.edits.get(&paths[0]), Some(&tone(0.5, 10.0)));
        flush(&mut app);
        assert_eq!(
            FolderStore::with_dir(dir.clone()).adjustments(&paths[0]),
            tone(0.5, 10.0),
            "the sidecar on disk carries the new values"
        );
        assert!(
            !app.edits.contains_key(&paths[1]),
            "only the selected photo changed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn applying_one_look_to_three_photos_keeps_each_crop() {
        let (mut app, dir, paths) = folder_app("three", 3);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        let cropped = Adjustments {
            crop: Some(CROP),
            ..Adjustments::default()
        };
        app.edits.insert(paths[1].clone(), cropped);
        app.folder_store.set_adjustments(&paths[1], &cropped);
        app.selected = (0..3).collect();
        app.copied_settings = Some(crate::app::folder_store::CopiedSettings {
            masks: Vec::new(),
            path: paths[0].clone(),
            adj: tone(0.25, 5.0),
            sections: CopySections::LOOK,
        });

        app.apply_settings_to_selection();

        flush(&mut app);
        let folder_store = FolderStore::with_dir(dir.clone());
        for path in &paths {
            let edit = app.edits.get(path).copied().unwrap();
            assert_eq!(edit.exposure, 0.25, "{} got the look", path.display());
            assert_eq!(edit.contrast, 5.0);
            assert_eq!(folder_store.adjustments(path).exposure, 0.25);
        }
        assert_eq!(
            app.edits.get(&paths[1]).unwrap().crop,
            Some(CROP),
            "the cropped photo keeps its own crop"
        );
        assert_eq!(app.edits.get(&paths[0]).unwrap().crop, None);
        assert_eq!(folder_store.adjustments(&paths[1]).crop, Some(CROP));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preset_that_merges_to_identity_removes_the_edit_entirely() {
        let (mut app, dir, paths) = folder_app("identity", 1);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.sel = Some(0);
        app.apply_adjustments(tone(0.7, 20.0));
        assert!(app.edits.contains_key(&paths[0]));

        app.presets
            .add("Neutral", Adjustments::default(), Vec::new());
        let id = app.presets.presets()[0].id;
        app.apply_preset(&PresetRef::Library(id));

        assert!(
            !app.edits.contains_key(&paths[0]),
            "an identity merge removes the entry rather than storing a no-op"
        );
        flush(&mut app);
        assert!(
            FolderStore::with_dir(dir.clone())
                .adjustments(&paths[0])
                .is_identity(),
            "and the sidecar no longer holds the old edit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_name_prompt_saves_what_was_typed_and_suffixes_a_duplicate() {
        let (mut app, dir, paths) = folder_app("prompt", 1);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.apply_adjustments(tone(0.3, 0.0));

        app.prompt_save_preset();
        assert_eq!(
            app.preset_name_edit(),
            Some(("Preset 1".to_string(), false)),
            "the prompt opens seeded with a suggestion"
        );
        app.set_preset_name_text("  Golden Hour  ".to_string());
        app.commit_preset_name();
        assert!(app.preset_name_edit().is_none(), "Enter closes the prompt");
        assert_eq!(app.presets.presets()[0].name, "Golden Hour");

        app.prompt_save_preset();
        app.set_preset_name_text(String::new());
        app.commit_preset_name();
        assert!(
            app.preset_name_edit().is_some(),
            "a blank name leaves the prompt open rather than saving nothing"
        );
        app.set_preset_name_text("golden hour".to_string());
        app.commit_preset_name();
        let names: Vec<&str> = app
            .presets
            .presets()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Golden Hour", "golden hour 2"]);

        app.prompt_save_preset();
        app.cancel_preset_name();
        assert!(app.preset_name_edit().is_none(), "Escape closes the prompt");
        assert_eq!(app.presets.presets().len(), 2, "cancel saves nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_through_the_prompt_keeps_the_id_and_reorders_the_row() {
        let (mut app, dir, _) = folder_app("rename", 1);
        app.presets.add("Zebra", tone(0.1, 0.0), Vec::new());
        app.presets.add("Mango", tone(0.2, 0.0), Vec::new());
        let zebra = app
            .presets
            .presets()
            .iter()
            .find(|p| p.name == "Zebra")
            .unwrap()
            .id;

        app.prompt_rename_preset(zebra);
        assert_eq!(app.preset_name_edit(), Some(("Zebra".to_string(), true)));
        app.set_preset_name_text("Alpine".to_string());
        app.commit_preset_name();

        let names: Vec<&str> = app
            .presets
            .presets()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Alpine", "Mango"], "the row moved into name order");
        assert_eq!(
            app.presets.get(zebra).map(|p| p.adjustments.exposure),
            Some(0.1),
            "the same id still carries the same look"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The center of `item`'s rail icon as last drawn.
    fn rail_icon(app: &App, item: RailItem) -> egui::Pos2 {
        crate::ui::rail_button_rect(&app.egui_ctx, item)
            .unwrap_or_else(|| panic!("the rail drew no {item:?} icon"))
            .center()
    }

    /// Clicks the rail's Presets icon and lands what the click asked for.
    /// Returns whether it asked for the page, and the page as it then paints.
    fn open_page(app: &mut App) -> (bool, Painted) {
        let _ = settled(app);
        let icon = rail_icon(app, RailItem::Presets);
        let (actions, _) = click(app, icon);
        let asked = actions.contains(&crate::ui::UiAction::ClickRail(RailItem::Presets));
        app.apply_ui_actions(actions);
        wait_for_scans(app);
        (asked, settled(app))
    }

    /// An App in the Grid with three photos selected, a library preset, and
    /// a preset folder already scanned.
    fn page_app(tag: &str) -> (App, PathBuf, Vec<PathBuf>, PathBuf) {
        let (mut app, dir, paths) = folder_app(tag, 3);
        let presets = preset_dir(
            &format!("{tag}-presets"),
            &[
                ("Film/Warm.xmp", lr_preset("Warm", "+0.5", "+10")),
                ("Travel/Night.xmp", lr_preset("Night", "-1", "0")),
            ],
        );
        app.add_preset_folder(&presets).unwrap();
        wait_for_scans(&mut app);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        app.mode = ViewMode::Grid;
        app.develop_open = true;
        app.selected = (0..3).collect();
        (app, dir, paths, presets)
    }

    /// The icon sits directly above Sliders in both views, as Lightroom's
    /// Presets icon sits above its Edit icon.
    #[test]
    fn the_presets_icon_is_first_in_the_rail_in_the_grid_and_the_loupe() {
        let (mut app, dir, paths, presets) = page_app("rail-icon");
        let sliders = RailItem::Develop(DevelopTab::Sliders);
        for mode in [ViewMode::Grid, ViewMode::Loupe] {
            app.mode = mode;
            if mode == ViewMode::Loupe {
                app.shown = Shown::Preview(paths[0].clone(), 100, 100);
            }
            let _ = settled(&mut app);
            let (above, below) = (rail_icon(&app, RailItem::Presets), rail_icon(&app, sliders));
            assert_eq!(above.x, below.x, "{mode:?}: one column");
            assert!(above.y < below.y, "{mode:?}: Presets is above Sliders");
            let others = [RailItem::Info, RailItem::Export];
            for other in others {
                assert!(
                    rail_icon(&app, other).y > below.y,
                    "{mode:?}: nothing comes between them"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// The pointer path from the rail icon to a look on three photos,
    /// driven as real clicks, since a wiring mistake between the widget and
    /// the action shows up nowhere else.
    #[test]
    fn a_page_row_click_applies_that_preset_to_the_selection() {
        let (mut app, dir, paths, presets) = page_app("page-click");
        let t = crate::i18n::t();

        std::fs::write(presets.join("Film/Fresh.xmp"), lr_preset("Fresh", "0", "0")).unwrap();
        let (asked, page) = open_page(&mut app);
        assert!(asked, "the icon asks for the Presets page");
        assert!(
            app.folder_presets().any(|p| p.name == "Fresh"),
            "opening the page reads the folders again"
        );
        assert_eq!(app.rail_lit(), Some(RailItem::Presets));
        assert!(
            app.develop_page_shown().is_none(),
            "the page takes the panel"
        );
        assert!(
            page.has(t.presets) && page.has(t.my_presets) && page.has("Golden"),
            "the page is headed Presets, the library open at the top: {:?}",
            page.texts()
        );
        assert!(
            page.has("Film") && page.has("Travel"),
            "each folder group has a header: {:?}",
            page.texts()
        );
        assert!(!page.has("Warm"), "a folder group starts folded");

        let (_, film) = click(&mut app, page.pos_of("Film"));
        let (actions, after) = click(&mut app, film.pos_of("Warm"));
        let warm = presets.join("Film/Warm.xmp");
        assert!(
            actions.contains(&crate::ui::UiAction::ApplyPreset(PresetRef::Folder(warm))),
            "the row asks to apply that preset: {actions:?}"
        );
        app.apply_ui_actions(actions);
        for path in &paths {
            assert_eq!(app.edits.get(path), Some(&tone(0.5, 10.0)), "{path:?}");
        }
        assert!(
            after.has("Golden") && app.presets_page_open(),
            "the page stays open for the next try: {:?}",
            after.texts()
        );

        let golden = app.presets.presets()[0].id;
        let (actions, _) = click(&mut app, after.pos_of("Golden"));
        assert!(
            actions.contains(&crate::ui::UiAction::ApplyPreset(PresetRef::Library(
                golden
            ))),
            "{actions:?}"
        );

        let sliders = RailItem::Develop(DevelopTab::Sliders);
        let icon = rail_icon(&app, sliders);
        let (actions, _) = click(&mut app, icon);
        app.apply_ui_actions(actions);
        assert_eq!(
            app.develop_page_shown(),
            Some(DevelopTab::Sliders),
            "Sliders goes back"
        );
        assert!(!app.presets_page_open());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// Catches rows that apply with nothing selected, and a library row
    /// that lost its Rename and Delete.
    #[test]
    fn the_page_rows_wait_for_a_selection_and_the_library_keeps_its_menu() {
        let (mut app, dir, _, presets) = page_app("page-menu");
        let t = crate::i18n::t();
        app.selected.clear();
        app.sel = None;

        let (_, page) = open_page(&mut app);
        let (actions, _) = click(&mut app, page.pos_of("Golden"));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, crate::ui::UiAction::ApplyPreset(_))),
            "a row with no photo selected does nothing: {actions:?}"
        );

        let dots = page.pos_of("\u{22ef}");
        let (actions, menu) = click(&mut app, dots);
        assert!(actions.is_empty(), "the row menu opens without acting");
        assert!(
            menu.has(t.rename) && menu.has(t.delete),
            "and offers rename and delete: {:?}",
            menu.texts()
        );
        let golden = app.presets.presets()[0].id;
        let (actions, _) = click(&mut app, menu.pos_of_near(t.delete, dots));
        assert!(
            actions.contains(&crate::ui::UiAction::RequestDeletePreset(golden)),
            "whose Delete asks for confirmation rather than deleting: {actions:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// The footer's entries reach their actions, and the search narrows
    /// the list across groups.
    #[test]
    fn the_page_footer_and_search_reach_their_actions() {
        let (mut app, dir, _, presets) = page_app("page-footer");
        let t = crate::i18n::t();

        let (_, page) = open_page(&mut app);
        assert!(page.has(t.preset_folders), "{:?}", page.texts());
        let (actions, _) = click(&mut app, page.pos_of(t.import_lr_presets));
        assert!(
            actions.contains(&crate::ui::UiAction::ImportLrPresets),
            "{actions:?}"
        );
        let (actions, _) = click(&mut app, page.pos_of(t.add_preset_folder));
        assert!(
            actions.contains(&crate::ui::UiAction::AddPresetFolder),
            "{actions:?}"
        );

        let _ = click(&mut app, page.pos_of(t.search_presets));
        let _ = frame(&mut app, vec![egui::Event::Text("nig".into())]);
        let found = settled(&mut app);
        assert!(
            found.has("Night") && !found.has("Golden") && !found.has("Warm"),
            "a search opens the groups with a match and hides the rest: {:?}",
            found.texts()
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// Catches a removal that leaves the folder's presets listed.
    #[test]
    fn removing_a_preset_folder_drops_its_presets() {
        let (mut app, dir, _, presets) = page_app("page-remove");
        assert_eq!(app.folder_presets().count(), 2);

        app.apply_ui_actions(vec![crate::ui::UiAction::RemovePresetFolder(
            presets.clone(),
        )]);

        assert!(app.catalog.preset_folders().is_empty());
        assert_eq!(app.folder_presets().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&presets);
    }

    /// The name prompt and the delete confirmation, drawn over the panel.
    #[test]
    fn the_preset_modals_paint_what_they_are_about() {
        let (mut app, dir, paths) = folder_app("modals", 1);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.develop_open = true;

        app.prompt_save_preset();
        let prompt = settled(&mut app);
        let t = crate::i18n::t();
        assert!(
            prompt.has(t.save_preset_title) && prompt.has("Preset 1"),
            "the prompt opens seeded with a suggestion: {:?}",
            prompt.texts()
        );

        app.cancel_preset_name();
        app.pending_confirm = Some(PendingConfirm::DeletePreset(app.presets.presets()[0].id));
        let confirm = settled(&mut app);
        assert!(
            confirm.any_containing("Golden"),
            "the delete confirmation names the preset: {:?}",
            confirm.texts()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches a name read from somewhere other than `crs:Name`, a missing stem
    /// fallback, and a dropped-field report that never reaches the durable
    /// channel the Develop panel shows on hover.
    #[test]
    fn importing_two_xmp_files_stores_both_looks_with_their_notes() {
        let (mut app, dir, _) = folder_app("lr-import", 1);
        let named = dir.join("warm.xmp");
        let unnamed = dir.join("gray.xmp");
        std::fs::write(&named, NAMED_PRESET).unwrap();
        std::fs::write(&unnamed, GRAYSCALE_PRESET).unwrap();

        app.add_lr_presets(&[named, unnamed]);

        let t = crate::i18n::t();
        let names: Vec<&str> = app
            .presets
            .presets()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["gray", "Warm Film"]);
        assert_eq!(
            app.status_text(),
            Some((t.lr_import_status)(2, 0, "").as_str()),
            "the status counts what landed and how much of it carries a note"
        );

        let warm = app
            .presets
            .presets()
            .iter()
            .find(|p| p.name == "Warm Film")
            .unwrap();
        assert_eq!(
            warm.adjustments,
            tone(0.35, 15.0),
            "the name comes from crs:Name and the sliders from the mapped fields"
        );
        assert_eq!(
            warm.notes,
            [(t.preset_import_note)("CropAngle")],
            "what Lightroom set and we cannot represent is stored with the look"
        );

        let gray = app
            .presets
            .presets()
            .iter()
            .find(|p| p.name == "gray")
            .unwrap();
        assert_eq!(gray.adjustments.exposure, -0.2);
        assert_eq!(
            gray.adjustments.saturation, -100.0,
            "B&W mode is approximated by pulling saturation out"
        );
        assert_eq!(gray.notes, [t.preset_monochrome_note]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches an import that abandons the rest of the batch on one bad file, a
    /// failure that never reaches the user, and a partial import that reports
    /// only one of its two halves. `set_status` replaces the message rather
    /// than appending, so a second call would drop whichever count came first.
    #[test]
    fn a_partial_import_reports_what_landed_as_well_as_what_did_not() {
        let (mut app, dir, _) = folder_app("lr-skip", 1);
        let junk = dir.join("notes.xmp");
        let also_junk = dir.join("recipe.xmp");
        let good = dir.join("warm.xmp");
        std::fs::write(&junk, "shopping list, not a preset").unwrap();
        std::fs::write(&also_junk, "nor is this one").unwrap();
        std::fs::write(&good, NAMED_PRESET).unwrap();

        app.add_lr_presets(&[junk, also_junk, good]);

        let names: Vec<&str> = app
            .presets
            .presets()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Warm Film"], "the readable file still lands");

        let t = crate::i18n::t();
        let first_failure = (t.lr_import_failed)("notes.xmp", "no rdf:Description");
        assert_eq!(
            app.status_text(),
            Some((t.lr_import_status)(1, 2, &first_failure).as_str()),
            "one message carries both counts and the first reason"
        );

        let status = app.status_text().unwrap();
        assert!(
            status.contains("notes.xmp"),
            "it names a file it could not read: {status}"
        );
        assert!(
            status.contains('1') && status.contains('2'),
            "and neither count is lost to the other: {status}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches a cancelled picker toasting "Imported 0 presets", which the
    /// counting status line would otherwise do on every dismissed dialog.
    #[test]
    fn a_cancelled_picker_says_nothing() {
        let (mut app, dir, _) = folder_app("lr-cancel", 1);

        app.add_lr_presets(&[]);

        assert_eq!(app.status_text(), None);
        assert!(app.presets.presets().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_preset_leaves_the_others() {
        let (mut app, dir, _) = folder_app("delete", 1);
        app.presets.add("Golden", tone(0.5, 0.0), Vec::new());
        app.presets.add("Moody", tone(-0.5, 0.0), Vec::new());
        let golden = app
            .presets
            .presets()
            .iter()
            .find(|p| p.name == "Golden")
            .unwrap()
            .id;

        app.delete_preset(golden);

        let left: Vec<&str> = app
            .presets
            .presets()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(left, ["Moody"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
