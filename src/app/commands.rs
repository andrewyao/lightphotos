//! The one dispatcher for [`Command`]: the keyboard, the menu bar and
//! `--drive` all run commands through [`App::run`].

use super::*;

use crate::command::{
    self, Blocker, Command, CommandError, FlagChange, Outcome, Photos, SelectWord, Selection, Step,
    View, Zoom,
};
use crate::sidecar::{ColorLabel, Flag};

impl App {
    /// Runs `cmd` if [`check`](Self::check) allows it.
    pub(crate) fn run(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        self.check(&cmd)?;
        if cmd == Command::Undo {
            let out = self.apply_command(cmd);
            self.request_redraw();
            return out;
        }
        // Every develop write of one command is one undo step.
        self.history.begin();
        let out = self.apply_command(cmd);
        self.history.end();
        self.save_edit_unless_dragging();
        self.request_redraw();
        out
    }

    /// For the keyboard and the menu. An error that only means there was
    /// nothing to act on does nothing, as a dead key always has. Any other
    /// shows in the status toast.
    pub(crate) fn run_ui(&mut self, cmd: Command) {
        if let Err(e) = self.run(cmd) {
            if !e.is_nothing_to_do() {
                self.set_status(StatusKind::Error, e.to_string());
            }
        }
    }

    /// Whether `cmd` can run now: nothing holds the input, its params are in
    /// range, and it has photos to act on.
    pub(crate) fn check(&self, cmd: &Command) -> Result<(), CommandError> {
        if let Some(by) = self.blocker(cmd) {
            return Err(CommandError::Busy { by });
        }
        let loupe = self.mode == ViewMode::Loupe;
        match cmd {
            Command::Zoom(_) if !loupe => {
                return Err(CommandError::NeedsView { view: View::Loupe });
            }
            Command::Rotate(p) if p.photos.is_selected() && !loupe => {
                return Err(CommandError::NeedsView { view: View::Loupe });
            }
            Command::View(p) if p.mode != View::Grid && self.selected_path().is_none() => {
                return Err(CommandError::NoPhotos);
            }
            Command::Navigate(_) if self.visible.is_empty() => return Err(CommandError::NoPhotos),
            Command::Develop(p) => p.validate()?,
            Command::Crop(p) => p.validate()?,
            _ => {}
        }
        if let Some(photos) = cmd.photos() {
            self.resolve(photos)?;
        }
        Ok(())
    }

    /// The modal that holds the input, in `handle_key`'s order. Rotate still
    /// works while cropping, as Cmd+[ does.
    fn blocker(&self, cmd: &Command) -> Option<Blocker> {
        self.input_blocker().filter(|&by| {
            !(by == Blocker::Crop && matches!(cmd, Command::Rotate(p) if p.photos.is_selected()))
        })
    }

    /// The modal that holds the keyboard, if any.
    pub(crate) fn input_blocker(&self) -> Option<Blocker> {
        Some(if self.tour_step().is_some() {
            Blocker::Tour
        } else if self.confirm_open() {
            Blocker::Confirm
        } else if self.copy_dialog.is_some() {
            Blocker::CopySettings
        } else if self.straighten_tool() != StraightenTool::Off {
            Blocker::Straighten
        } else if self.cropping() {
            Blocker::Crop
        } else if self.tool == LoupeTool::WbPicker {
            Blocker::WbPicker
        } else if self.tool == LoupeTool::TouchUp {
            Blocker::TouchUp
        } else if self.preset_name_edit.is_some() {
            Blocker::PresetName
        } else if self.show_settings {
            Blocker::Settings
        } else {
            return None;
        })
    }

    /// The photos `photos` names. `Selected` is what the keyboard acts on.
    /// A name is an absolute path or a file name in the open folder.
    pub(crate) fn resolve(&self, photos: &Photos) -> Result<Vec<PathBuf>, CommandError> {
        let paths = match photos {
            Photos::Selected => {
                let paths = self.action_paths();
                if paths.is_empty() {
                    self.selected_path().into_iter().collect()
                } else {
                    paths
                }
            }
            Photos::Named(names) => names
                .iter()
                .map(|name| self.resolve_one(name))
                .collect::<Result<_, _>>()?,
        };
        if paths.is_empty() {
            return Err(CommandError::NoPhotos);
        }
        Ok(paths)
    }

    /// The playlist is one folder, so a file name names at most one photo.
    fn resolve_one(&self, name: &Path) -> Result<PathBuf, CommandError> {
        let entries = self.playlist.as_ref().map_or(&[][..], |pl| pl.entries());
        entries
            .iter()
            .find(|e| {
                if name.is_absolute() {
                    e.as_path() == name
                } else {
                    e.file_name() == Some(name.as_os_str())
                }
            })
            .cloned()
            .ok_or_else(|| CommandError::NoSuchPhoto {
                name: name.display().to_string(),
            })
    }

    fn apply_command(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        let done = |photos: usize| {
            Ok(Outcome {
                photos,
                pending: false,
            })
        };
        match cmd {
            Command::OpenFolder(p) => {
                self.open_folder_at(p.path)?;
                done(0)
            }
            Command::View(p) => {
                match p.mode {
                    View::Grid => self.enter_grid(),
                    View::Loupe => self.enter_loupe(),
                    View::Develop => self.open_develop(),
                }
                done(0)
            }
            Command::Zoom(p) => {
                match p.zoom {
                    Zoom::Fit => self.fit_to_window(),
                    Zoom::Actual => self.reset_100(),
                    Zoom::In => self.zoom_by(ZOOM_STEP),
                    Zoom::Out => self.zoom_by(1.0 / ZOOM_STEP),
                    Zoom::Cycle => self.cycle_zoom(),
                }
                done(0)
            }
            Command::Select(p) => self.select_photos(&p.photos),
            Command::Navigate(p) => {
                self.navigate(p.to);
                done(1)
            }
            Command::Rate(p) => {
                let paths = self.resolve(&p.photos)?;
                if p.photos.is_selected() {
                    self.rate_key(p.stars.get(), p.advance);
                } else {
                    for path in &paths {
                        self.set_rating_of(path.clone(), p.stars.get());
                    }
                }
                done(paths.len())
            }
            Command::Flag(p) => {
                let paths = self.resolve(&p.photos)?;
                let flag = match p.flag {
                    FlagChange::Pick => Some(Flag::Pick),
                    FlagChange::Reject => Some(Flag::Reject),
                    FlagChange::None => None,
                    FlagChange::TogglePick => {
                        let all = paths.iter().all(|p| self.flag_of(p) == Some(Flag::Pick));
                        (!all).then_some(Flag::Pick)
                    }
                };
                match p.photos {
                    Photos::Selected if p.flag == FlagChange::TogglePick => self.toggle_pick(),
                    Photos::Selected => self.set_flag(flag),
                    Photos::Named(_) => {
                        for path in &paths {
                            self.catalog.set_flag(path, flag);
                        }
                        self.after_flag_change();
                    }
                }
                done(paths.len())
            }
            Command::Label(p) => {
                let paths = self.resolve(&p.photos)?;
                self.set_label_of(&paths, p.label, p.on);
                done(paths.len())
            }
            Command::Rotate(p) => {
                if p.photos.is_selected() {
                    self.rotate(p.clockwise);
                    return done(1);
                }
                let paths = self.resolve(&p.photos)?;
                for path in &paths {
                    self.rotate_path(path, p.clockwise);
                }
                done(paths.len())
            }
            Command::Develop(p) => {
                let paths = self.resolve(&p.photos)?;
                done(self.edit_each(&paths, |adj| p.apply(adj)))
            }
            Command::Reset(p) => {
                let paths = self.resolve(&p.photos)?;
                done(self.edit_each(&paths, command::reset))
            }
            Command::Crop(p) => {
                let paths = self.resolve(&p.photos)?;
                // Every photo's edit is checked before any is written.
                let edits = paths
                    .iter()
                    .map(|path| {
                        let adj = self.edits.get(path).copied().unwrap_or_default();
                        p.apply(adj, || self.photo_size(path))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for (path, adj) in paths.iter().zip(edits) {
                    self.edit_each(std::slice::from_ref(path), |_| adj);
                }
                let n = paths.len();
                if self.fitted
                    && self
                        .shown
                        .path()
                        .is_some_and(|s| paths.iter().any(|p| p == s))
                {
                    self.fit_to_window();
                }
                done(n)
            }
            Command::Monochrome(p) => {
                let paths = self.resolve(&p.photos)?;
                let on = p.on.unwrap_or_else(|| {
                    !paths
                        .iter()
                        .all(|p| self.edits.get(p).is_some_and(Adjustments::is_monochrome))
                });
                let saturation = if on { -100.0 } else { 0.0 };
                done(self.edit_each(&paths, |adj| Adjustments { saturation, ..adj }))
            }
            Command::AutoTone(p) => {
                let paths = self.resolve(&p.photos)?;
                if p.photos.is_selected() {
                    self.auto_tone_selected();
                } else {
                    self.auto_tone_batch(paths.clone(), autotone::DeferredAutoToneMode::Append);
                }
                Ok(Outcome {
                    photos: paths.len(),
                    pending: self.autotone.is_running() || self.catalog_loading(),
                })
            }
            Command::Preset(p) => {
                let paths = self.resolve(&p.photos)?;
                let Some(preset) = self.presets.presets().iter().find(|s| s.name == p.name) else {
                    let names: Vec<&str> = self
                        .presets
                        .presets()
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    return Err(CommandError::Failed {
                        reason: format!(
                            "no preset named `{}`; the presets are: {}",
                            p.name,
                            names.join(", ")
                        ),
                    });
                };
                let look = preset.adjustments;
                let n = self.apply_look_to(&look, crate::develop::copy::CopySections::LOOK, &paths);
                done(n)
            }
            Command::Export(p) => {
                let paths = self.resolve(&p.photos)?;
                let pending = self
                    .export_with(paths.clone(), p.settings)
                    .map_err(|why| CommandError::Failed { reason: why.into() })?;
                Ok(Outcome {
                    photos: paths.len(),
                    pending,
                })
            }
            Command::Undo => done(self.undo_edit()),
        }
    }

    /// `path`'s size in display pixels, which a straightened crop needs.
    /// Read from the file when the app has not loaded it yet.
    fn photo_size(&self, path: &Path) -> Result<(f32, f32), CommandError> {
        let from_meta = |m: &image_decode::ImageMetadata| m.source_size;
        let size = match self.exif_cache.get(path).and_then(from_meta) {
            Some((w, h)) => Some((w as f32, h as f32)),
            None if self.shown.path() == Some(path) => Some(self.image_size()),
            None => {
                from_meta(&image_decode::read_metadata(path)).map(|(w, h)| (w as f32, h as f32))
            }
        };
        size.filter(|&(w, h)| w > 0.0 && h > 0.0)
            .ok_or_else(|| CommandError::Failed {
                reason: format!(
                    "cannot read the size of {}, which a straightened crop needs",
                    path.display()
                ),
            })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_folder_at(&mut self, path: PathBuf) -> Result<(), CommandError> {
        if !path.is_dir() {
            return Err(CommandError::Failed {
                reason: format!("{} is not a folder", path.display()),
            });
        }
        self.finish_folder_jobs();
        self.load_folder(path);
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn open_folder_at(&mut self, _path: PathBuf) -> Result<(), CommandError> {
        Err(CommandError::Failed {
            reason: "the browser opens folders only through its folder picker".into(),
        })
    }

    fn select_photos(&mut self, photos: &Selection) -> Result<Outcome, CommandError> {
        match photos {
            Selection::Word(SelectWord::All) => self.select_all(),
            Selection::Word(SelectWord::None) => self.deselect_all(),
            Selection::Named(names) => {
                let paths = self.resolve(&Photos::Named(names.clone()))?;
                let mut cells = BTreeSet::new();
                for path in &paths {
                    let cell = self
                        .playlist
                        .as_ref()
                        .and_then(|pl| pl.entries().iter().position(|e| e == path))
                        .and_then(|idx| self.place_of(idx).cell())
                        .ok_or_else(|| CommandError::Failed {
                            reason: format!(
                                "{} is not shown: a filter or a closed stack hides it",
                                path.display()
                            ),
                        })?;
                    cells.insert(cell);
                }
                self.sel = cells.iter().next().copied();
                self.anchor = self.sel;
                self.selected = cells;
                if self.mode == ViewMode::Loupe {
                    self.load_selected();
                    self.request_neighbors();
                }
            }
        }
        Ok(Outcome {
            photos: self.selection_count(),
            pending: false,
        })
    }

    fn navigate(&mut self, to: Step) {
        let last = self.visible.len().saturating_sub(1);
        match (to, self.mode) {
            (Step::Next, ViewMode::Loupe) => self.step_loupe(true),
            (Step::Previous, ViewMode::Loupe) => self.step_loupe(false),
            (Step::Next, ViewMode::Grid) => self.move_grid(1, 0),
            (Step::Previous, ViewMode::Grid) => self.move_grid(-1, 0),
            (Step::First | Step::Last, mode) => {
                self.select_single(if to == Step::First { 0 } else { last });
                if mode == ViewMode::Loupe {
                    self.load_selected();
                    self.request_neighbors();
                }
            }
        }
    }

    /// Sets `label` on each path, or clears it where it is set. `on: None`
    /// toggles: on, or off when every path has it already.
    pub(super) fn set_label_of(&mut self, paths: &[PathBuf], label: ColorLabel, on: Option<bool>) {
        let on = on.unwrap_or_else(|| !paths.iter().all(|p| self.catalog.label(p) == Some(label)));
        for path in paths {
            if on {
                self.catalog.set_label(path, Some(label));
            } else if self.catalog.label(path) == Some(label) {
                self.catalog.set_label(path, None);
            }
        }
        if !self.label_filter.is_empty() {
            self.refilter_after_catalog_change();
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    fn rotate_path(&mut self, path: &Path, cw: bool) {
        if self.shown.path() == Some(path) && self.mode == ViewMode::Loupe {
            self.rotate(cw);
            return;
        }
        let now = self.rotations.get(path).copied().unwrap_or(0);
        let step = (now % 4 + if cw { 1 } else { 3 }) % 4;
        if step == 0 {
            self.rotations.remove(path);
        } else {
            self.rotations.insert(path.to_path_buf(), step);
        }
        self.catalog.set_rotation(path, step);
    }
}

/// A zoom in or out step, as Cmd+= and Cmd+- take.
const ZOOM_STEP: f32 = 1.2;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::folder_app;
    use crate::command::{CropParams, DevelopParams, FlagParams, LabelParams, RateParams, Stars};
    use crate::develop::{Crop, SliderId, Straighten};

    fn run(app: &mut App, json: &str) -> Result<Outcome, CommandError> {
        app.run(Command::parse(json)?)
    }

    /// The test photos are empty files, so give them a size to straighten.
    fn sized(app: &mut App, paths: &[PathBuf], size: (u32, u32)) {
        for path in paths {
            let meta = image_decode::ImageMetadata {
                source_size: Some(size),
                ..Default::default()
            };
            app.exif_cache.insert(path.clone(), meta);
        }
    }

    /// Headless mode edits a sidecar through the lib. It must leave the same
    /// sidecar the app leaves for the same command on the same photo.
    #[test]
    fn develop_set_on_a_record_matches_the_app() {
        use crate::sidecar::{self, ImageRecord};
        let (mut app, dir, paths) = folder_app("cmd-headless-parity", 2);
        let mut start = ImageRecord {
            rating: Some(3),
            ..ImageRecord::default()
        };
        start.edit(1_700_000_000, |r| {
            r.adjustments.exposure = 0.5;
            r.adjustments.contrast = 10.0;
        });
        sidecar::write_record(&paths[0], &start).unwrap();
        crate::app::test_support::load_folder(&mut app, &dir);

        let json = r#"{"id":"develop.set","params":{"photos":["0.jpg"],
            "controls":{"exposure":{"by":0.3},"contrast":"default","highlights":-20}}}"#;
        let cmd = Command::parse(json).unwrap();
        let mut headless = start.clone();
        command::apply_to_records(&cmd, std::slice::from_mut(&mut headless), |_| {
            Ok((4000.0, 3000.0))
        })
        .unwrap();
        assert_eq!(app.run(cmd).map(|o| o.photos), Ok(1));
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));

        let saved = sidecar::read_record(&paths[0]).unwrap();
        assert_ne!(saved.adjustments, start.adjustments, "the app edited it");
        assert!(saved.edited.is_some() && headless.edited.is_some());
        assert_eq!(
            ImageRecord {
                edited: None,
                ..saved
            },
            ImageRecord {
                edited: None,
                ..headless
            }
        );
    }

    /// A toggle on several photos is one decision for the set, in the app
    /// and headless alike: a mixed set turns on, a uniform one turns off.
    #[test]
    fn toggles_on_a_mixed_set_match_the_app() {
        use crate::sidecar::{self, ColorLabel, Flag, ImageRecord};
        let mut mono = ImageRecord::default();
        mono.edit(1_700_000_000, |r| r.adjustments.saturation = -100.0);
        let mixed = [
            ImageRecord {
                flag: Some(Flag::Pick),
                label: Some(ColorLabel::Red),
                ..mono
            },
            ImageRecord::default(),
        ];
        type Field = fn(&ImageRecord) -> String;
        let cases: [(&str, Field); 3] = [
            (
                r#"{"id":"photo.flag","params":{"flag":"toggle_pick","photos":["0.jpg","1.jpg"]}}"#,
                |r| format!("{:?}", r.flag),
            ),
            (
                r#"{"id":"photo.label","params":{"label":"red","photos":["0.jpg","1.jpg"]}}"#,
                |r| format!("{:?}", r.label),
            ),
            (
                r#"{"id":"develop.monochrome","params":{"photos":["0.jpg","1.jpg"]}}"#,
                |r| r.adjustments.is_monochrome().to_string(),
            ),
        ];
        for (json, field) in cases {
            let (mut app, dir, paths) = folder_app("cmd-toggle-parity", 2);
            for (path, rec) in paths.iter().zip(&mixed) {
                sidecar::write_record(path, rec).unwrap();
            }
            crate::app::test_support::load_folder(&mut app, &dir);

            let cmd = Command::parse(json).unwrap();
            let mut headless = mixed.clone();
            command::apply_to_records(&cmd, &mut headless, |_| Ok((4000.0, 3000.0))).unwrap();
            assert_eq!(app.run(cmd).map(|o| o.photos), Ok(2), "{json}");
            app.catalog
                .flush_blocking(std::time::Duration::from_secs(10));

            let saved: Vec<String> = paths
                .iter()
                .map(|p| field(&sidecar::read_record(p).unwrap()))
                .collect();
            let settled: Vec<String> = headless.iter().map(field).collect();
            assert_eq!(saved, settled, "{json}");
            assert_eq!(saved[0], saved[1], "one value for the set: {json}");
        }
    }

    /// A sidecar comes from whoever supplied the folder, and its rotation is
    /// any u8. Turning such a photo wraps instead of overflowing.
    #[test]
    fn rotating_a_photo_with_a_hostile_saved_rotation_wraps() {
        let (mut app, dir, paths) = folder_app("cmd-rotate-hostile", 1);
        let side = dir.join(crate::sidecar::SIDECAR_DIR);
        std::fs::create_dir_all(&side).unwrap();
        std::fs::write(side.join("0.jpg.xmp"), r#"{"rotation":255}"#).unwrap();
        crate::app::test_support::load_folder(&mut app, &dir);

        let cw = r#"{"id":"photo.rotate","params":{"clockwise":true,"photos":["0.jpg"]}}"#;
        assert_eq!(run(&mut app, cw).map(|o| o.photos), Ok(1));
        assert_eq!(app.catalog.rotation(&paths[0]), 0, "3 and a quarter turn");
    }

    #[test]
    fn a_crop_set_that_fails_one_photo_writes_none() {
        let (mut app, _, paths) = folder_app("cmd-crop-atomic", 2);
        sized(&mut app, &paths[..1], (4000, 3000));
        app.select_all();
        let err = run(&mut app, r#"{"id":"crop.set","params":{"straighten":4}}"#).unwrap_err();
        assert!(
            matches!(&err, CommandError::Failed { reason } if reason.contains("1.jpg")),
            "{err}"
        );
        for path in &paths {
            assert!(
                app.catalog.adjustments(path).is_identity(),
                "{}",
                path.display()
            );
        }

        sized(&mut app, &paths, (3000, 4000));
        let err = run(
            &mut app,
            r#"{"id":"crop.set","params":{"straighten":4,"crop":{"left":0,"top":0,"right":1,"bottom":0.98}}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(&err, CommandError::BadParams { reason, .. } if reason.contains("4° straighten")),
            "{err}"
        );
        for path in &paths {
            assert!(
                app.catalog.adjustments(path).is_identity(),
                "{}",
                path.display()
            );
        }
    }

    fn named(paths: &[PathBuf]) -> Photos {
        Photos::Named(
            paths
                .iter()
                .map(|p| PathBuf::from(p.file_name().unwrap()))
                .collect(),
        )
    }

    #[test]
    fn rate_acts_on_the_selection_or_on_named_photos() {
        let (mut app, _, paths) = folder_app("cmd-rate", 3);
        app.select_single(1);
        assert_eq!(
            run(&mut app, r#"{"id":"photo.rate","params":{"stars":4}}"#),
            Ok(Outcome {
                photos: 1,
                pending: false
            })
        );
        assert_eq!((app.rating_of(&paths[0]), app.rating_of(&paths[1])), (0, 4));

        let cmd = Command::Rate(RateParams {
            stars: Stars::try_from(2).unwrap(),
            advance: false,
            photos: Photos::Named(vec!["0.jpg".into(), paths[2].clone()]),
        });
        assert_eq!(
            app.run(cmd).map(|o| o.photos),
            Ok(2),
            "a bare name and a path"
        );
        let ratings: Vec<u8> = paths.iter().map(|p| app.rating_of(p)).collect();
        assert_eq!(ratings, [2, 4, 2]);
    }

    #[test]
    fn flag_and_label_act_like_their_keys() {
        let (mut app, _, paths) = folder_app("cmd-flag", 2);
        app.select_single(0);
        run(
            &mut app,
            r#"{"id":"photo.flag","params":{"flag":"reject"}}"#,
        )
        .unwrap();
        assert_eq!(app.flag_of(&paths[0]), Some(Flag::Reject));
        run(
            &mut app,
            r#"{"id":"photo.flag","params":{"flag":"toggle_pick"}}"#,
        )
        .unwrap();
        assert_eq!(app.flag_of(&paths[0]), Some(Flag::Pick));
        run(
            &mut app,
            r#"{"id":"photo.flag","params":{"flag":"toggle_pick"}}"#,
        )
        .unwrap();
        assert_eq!(app.flag_of(&paths[0]), None, "a second toggle unflags");

        let label = |on| {
            Command::Label(LabelParams {
                label: ColorLabel::Green,
                on,
                photos: named(&paths),
            })
        };
        app.run(label(None)).unwrap();
        assert!(paths
            .iter()
            .all(|p| app.catalog.label(p) == Some(ColorLabel::Green)));
        app.run(label(Some(true))).unwrap();
        assert!(
            paths
                .iter()
                .all(|p| app.catalog.label(p) == Some(ColorLabel::Green)),
            "on: true keeps it on"
        );
        app.run(label(None)).unwrap();
        assert!(
            paths.iter().all(|p| app.catalog.label(p).is_none()),
            "toggled off"
        );
        assert_eq!(
            app.flag_of(&paths[1]),
            None,
            "the label left the flag alone"
        );
        let flag = Command::Flag(FlagParams {
            flag: FlagChange::Pick,
            photos: named(&paths[1..]),
        });
        app.run(flag).unwrap();
        assert_eq!(
            (app.flag_of(&paths[0]), app.flag_of(&paths[1])),
            (None, Some(Flag::Pick))
        );
    }

    #[test]
    fn develop_set_writes_every_control_to_each_photo() {
        let (mut app, _, paths) = folder_app("cmd-develop", 3);
        app.selected = BTreeSet::from([0, 1]);
        app.sel = Some(0);
        let out = run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"exposure":0.5,"contrast":20}}}"#,
        );
        assert_eq!(out.map(|o| o.photos), Ok(2));
        for path in &paths[..2] {
            let adj = app.catalog.adjustments(path);
            assert_eq!(
                (adj.exposure, adj.contrast),
                (0.5, 20.0),
                "{}",
                path.display()
            );
        }
        assert!(app.catalog.adjustments(&paths[2]).is_identity());
    }

    #[test]
    fn develop_set_with_one_value_out_of_range_changes_nothing() {
        let (mut app, _, paths) = folder_app("cmd-atomic", 2);
        app.select_single(0);
        let err = run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"contrast":20,"exposure":9}}}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CommandError::OutOfRange {
                control: "exposure".into(),
                value: 9.0,
                min: -5.0,
                max: 5.0
            }
        );
        assert!(err.to_string().contains("-5 to 5"), "{err}");
        assert!(
            !app.edits.contains_key(&paths[0]),
            "contrast was not written either"
        );
        assert!(app.catalog.adjustments(&paths[0]).is_identity());
    }

    #[test]
    fn crop_set_crops_and_straightens_and_the_full_frame_clears_it() {
        let (mut app, _, paths) = folder_app("cmd-crop", 1);
        sized(&mut app, &paths, (4000, 3000));
        app.select_single(0);
        let crop = Crop {
            left: 0.1,
            top: 0.2,
            right: 0.9,
            bottom: 0.8,
        };
        app.run(Command::Crop(CropParams {
            crop: Some(crop),
            straighten: Some(2.0),
            photos: Photos::Selected,
        }))
        .unwrap();
        let adj = app.catalog.adjustments(&paths[0]);
        assert_eq!((adj.crop, adj.straighten), (Some(crop), 2.0));

        run(
            &mut app,
            r#"{"id":"crop.set","params":{"crop":{"left":0,"top":0,"right":1,"bottom":1}}}"#,
        )
        .unwrap();
        let adj = app.catalog.adjustments(&paths[0]);
        assert_eq!(
            (adj.crop, adj.straighten),
            (
                Some(Straighten::new(2.0, 4000.0, 3000.0).largest_crop(4000.0 / 3000.0)),
                2.0
            ),
            "straighten stays, so the cleared crop is the largest that fits it"
        );
        assert!(matches!(
            run(&mut app, r#"{"id":"crop.set","params":{"straighten":90}}"#),
            Err(CommandError::OutOfRange { .. })
        ));
    }

    #[test]
    fn a_name_resolves_to_one_photo_or_says_why_not() {
        let (app, dir, paths) = folder_app("cmd-names", 2);
        assert_eq!(app.resolve_one(&paths[1]), Ok(paths[1].clone()));
        assert_eq!(app.resolve_one(Path::new("1.jpg")), Ok(paths[1].clone()));
        assert_eq!(
            app.resolve_one(&dir.join("elsewhere").join("1.jpg")),
            Err(CommandError::NoSuchPhoto {
                name: dir.join("elsewhere").join("1.jpg").display().to_string()
            }),
            "a full path must match exactly"
        );
        assert_eq!(
            app.resolve_one(Path::new("y.jpg")),
            Err(CommandError::NoSuchPhoto {
                name: "y.jpg".into()
            })
        );
        assert_eq!(
            app.resolve(&Photos::Selected),
            Err(CommandError::NoPhotos),
            "nothing selected"
        );
    }

    #[test]
    fn a_dialog_holds_commands_as_it_holds_their_keys() {
        let (mut app, _, paths) = folder_app("cmd-busy", 2);
        app.select_single(0);
        app.request_bulk(ui::BulkKind::Delete);
        assert!(app.confirm_open());
        let rate = r#"{"id":"photo.rate","params":{"stars":3}}"#;
        assert_eq!(
            run(&mut app, rate),
            Err(CommandError::Busy {
                by: Blocker::Confirm
            })
        );
        app.modifiers = ModifiersState::empty();
        app.handle_key(KeyCode::Digit3);
        assert_eq!(app.rating_of(&paths[0]), 0);
        app.cancel_pending();

        app.show_settings = true;
        assert_eq!(
            run(&mut app, rate),
            Err(CommandError::Busy {
                by: Blocker::Settings
            })
        );
        app.show_settings = false;
        run(&mut app, rate).unwrap();
        assert_eq!(app.rating_of(&paths[0]), 3);
    }

    /// The keyboard's modal returns and `blocker` both encode which modal
    /// holds the input. For each modal a test can open, a command key does
    /// nothing exactly when its command is Busy.
    #[test]
    fn every_modal_that_holds_the_keys_holds_their_commands() {
        let keyed = [
            (
                KeyCode::Digit3,
                r#"{"id":"photo.rate","params":{"stars":3}}"#,
            ),
            (
                KeyCode::KeyP,
                r#"{"id":"photo.flag","params":{"flag":"pick"}}"#,
            ),
            (
                KeyCode::Digit6,
                r#"{"id":"photo.label","params":{"label":"red"}}"#,
            ),
            (KeyCode::KeyV, r#"{"id":"develop.monochrome"}"#),
            (
                KeyCode::KeyG,
                r#"{"id":"view.mode","params":{"mode":"grid"}}"#,
            ),
        ];
        type Open = fn(&mut App);
        let open: [(&str, Open); 7] = [
            ("nothing", |_| {}),
            ("confirm", |app| app.request_bulk(ui::BulkKind::Delete)),
            ("copy settings", |app| app.copy_settings()),
            ("settings", |app| app.show_settings = true),
            ("preset name", |app| {
                app.preset_name_edit = Some(("x".into(), None));
            }),
            ("touch up", |app| app.tool = LoupeTool::TouchUp),
            ("wb picker", |app| app.tool = LoupeTool::WbPicker),
        ];
        for (name, setup) in open {
            for (code, json) in keyed {
                let (mut app, _, paths) = folder_app("cmd-modal", 2);
                app.mode = ViewMode::Loupe;
                app.select_single(0);
                app.want = Some(paths[0].clone());
                setup(&mut app);
                let busy = matches!(
                    app.check(&Command::parse(json).unwrap()),
                    Err(CommandError::Busy { .. })
                );
                let before = (
                    app.rating_of(&paths[0]),
                    app.flag_of(&paths[0]),
                    app.catalog.label(&paths[0]),
                    app.edits.len(),
                    app.mode,
                );
                app.modifiers = ModifiersState::empty();
                app.handle_key(code);
                let after = (
                    app.rating_of(&paths[0]),
                    app.flag_of(&paths[0]),
                    app.catalog.label(&paths[0]),
                    app.edits.len(),
                    app.mode,
                );
                assert_eq!(before == after, busy, "{name}, {code:?}: busy {busy}");
            }
        }
    }

    #[test]
    fn zoom_needs_the_loupe() {
        let (mut app, _, _) = folder_app("cmd-zoom", 1);
        assert_eq!(
            run(&mut app, r#"{"id":"view.zoom","params":{"zoom":"in"}}"#),
            Err(CommandError::NeedsView { view: View::Loupe })
        );
    }

    #[test]
    fn select_and_view_move_the_app_like_their_keys() {
        let (mut app, _, paths) = folder_app("cmd-select", 3);
        run(
            &mut app,
            r#"{"id":"select","params":{"photos":["2.jpg","1.jpg"]}}"#,
        )
        .unwrap();
        assert_eq!(
            app.selected_paths(),
            vec![paths[1].clone(), paths[2].clone()]
        );
        run(&mut app, r#"{"id":"select","params":{"photos":"none"}}"#).unwrap();
        assert_eq!(app.selection_count(), 0);
        assert_eq!(
            run(&mut app, r#"{"id":"view.mode","params":{"mode":"loupe"}}"#),
            Err(CommandError::NoPhotos)
        );
        run(&mut app, r#"{"id":"navigate","params":{"to":"last"}}"#).unwrap();
        run(&mut app, r#"{"id":"view.mode","params":{"mode":"loupe"}}"#).unwrap();
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.selected_path(), Some(paths[2].clone()));
    }

    #[test]
    fn one_develop_set_of_several_controls_undoes_in_one_step() {
        let (mut app, _, paths) = folder_app("cmd-undo", 2);
        app.select_all();
        let set = r#"{"id":"develop.set","params":{"controls":{"exposure":0.5,"contrast":20}}}"#;
        run(&mut app, set).unwrap();
        run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"clarity":30},"photos":["1.jpg"]}}"#,
        )
        .unwrap();

        assert_eq!(
            run(&mut app, r#"{"id":"edit.undo"}"#).map(|o| o.photos),
            Ok(1)
        );
        let adj = app.catalog.adjustments(&paths[1]);
        assert_eq!((adj.exposure, adj.contrast, adj.clarity), (0.5, 20.0, 0.0));

        assert_eq!(
            run(&mut app, r#"{"id":"edit.undo"}"#).map(|o| o.photos),
            Ok(2)
        );
        for path in &paths {
            assert!(
                app.catalog.adjustments(path).is_identity(),
                "{}",
                path.display()
            );
            assert!(!app.edits.contains_key(path));
        }
        assert_eq!(
            run(&mut app, r#"{"id":"edit.undo"}"#).map(|o| o.photos),
            Ok(0),
            "nothing left to undo"
        );
    }

    #[test]
    fn undo_takes_back_a_step_the_pointer_still_holds_open() {
        let (mut app, _, paths) = folder_app("cmd-undo-open", 1);
        app.select_single(0);
        run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"exposure":1}}}"#,
        )
        .unwrap();
        let dragged = Adjustments {
            exposure: 2.0,
            ..Default::default()
        };
        app.put_edit(&paths[0], dragged);
        assert_eq!(
            run(&mut app, r#"{"id":"edit.undo"}"#).map(|o| o.photos),
            Ok(1)
        );
        assert_eq!(app.catalog.adjustments(&paths[0]).exposure, 1.0);
        assert_eq!(app.edits.get(&paths[0]).map(|a| a.exposure), Some(1.0));
    }

    #[test]
    fn undo_of_reset_all_puts_back_the_touch_ups() {
        let (mut app, _, paths) = folder_app("cmd-undo-reset-all", 1);
        app.select_single(0);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        let spot = crate::develop::TouchUp {
            center: [0.5, 0.5],
            radius: 0.05,
            source: [0.3, 0.3],
            feather: 0.5,
            delta: [0.0; 3],
            opacity: 1.0,
        };
        app.apply_touchups(vec![spot]);
        run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"exposure":1}}}"#,
        )
        .unwrap();

        app.apply_ui_actions(vec![ui::UiAction::ResetAllEdits]);
        app.save_edit_unless_dragging();
        assert!(app.catalog.touchups(&paths[0]).is_empty());

        assert_eq!(
            run(&mut app, r#"{"id":"edit.undo"}"#).map(|o| o.photos),
            Ok(1)
        );
        assert_eq!(app.catalog.adjustments(&paths[0]).exposure, 1.0);
        assert_eq!(app.catalog.touchups(&paths[0]), vec![spot]);
        assert_eq!(app.current_touchups(), &[spot]);

        run(&mut app, r#"{"id":"edit.undo"}"#).unwrap();
        assert_eq!(
            app.catalog.touchups(&paths[0]),
            vec![spot],
            "undoing the exposure leaves the spots"
        );
    }

    #[test]
    fn undo_puts_back_a_crop_and_skips_ratings() {
        let (mut app, _, paths) = folder_app("cmd-undo-crop", 1);
        sized(&mut app, &paths, (4000, 3000));
        app.select_single(0);
        run(&mut app, r#"{"id":"crop.set","params":{"straighten":3}}"#).unwrap();
        assert!(
            app.catalog.adjustments(&paths[0]).crop.is_some(),
            "a turned photo keeps a box"
        );
        run(&mut app, r#"{"id":"photo.rate","params":{"stars":5}}"#).unwrap();
        run(&mut app, r#"{"id":"edit.undo"}"#).unwrap();
        let adj = app.catalog.adjustments(&paths[0]);
        assert_eq!((adj.crop, adj.straighten), (None, 0.0));
        assert_eq!(
            app.rating_of(&paths[0]),
            5,
            "undo covers develop edits only"
        );
    }

    #[test]
    fn a_new_folder_forgets_the_undo_steps() {
        let (mut app, dir, paths) = folder_app("cmd-undo-folder", 1);
        app.select_single(0);
        run(
            &mut app,
            r#"{"id":"develop.set","params":{"controls":{"exposure":1}}}"#,
        )
        .unwrap();
        crate::app::test_support::load_folder(&mut app, &dir);
        assert!(!app.history.can_undo());
        assert_eq!(app.catalog.adjustments(&paths[0]).exposure, 1.0);
    }

    #[test]
    fn develop_reset_keeps_the_crop() {
        let (mut app, _, paths) = folder_app("cmd-reset", 1);
        app.select_single(0);
        let mut controls = std::collections::BTreeMap::new();
        controls.insert(SliderId::Exposure, crate::command::ControlValue::To(1.0));
        app.run(Command::Develop(DevelopParams {
            controls,
            photos: Photos::Selected,
        }))
        .unwrap();
        run(
            &mut app,
            r#"{"id":"crop.set","params":{"crop":{"left":0.1,"top":0.1,"right":0.9,"bottom":0.9}}}"#,
        )
        .unwrap();
        run(&mut app, r#"{"id":"develop.reset"}"#).unwrap();
        let adj = app.catalog.adjustments(&paths[0]);
        assert_eq!(adj.exposure, 0.0);
        assert!(adj.crop.is_some());
    }
}
