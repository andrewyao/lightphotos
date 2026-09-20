use super::*;

// The Develop panel and the selection bar reach these in the commit that draws
// them; until then only this module's own tests do.
#[cfg_attr(not(test), allow(dead_code))]
impl App {
    /// Saves the shown photo's tone settings under `name`, with no crop,
    /// rotation or touch-ups, so the look can be applied to any other photo.
    pub(super) fn save_preset_from_shown(&mut self, name: &str) {
        let tone = self.current_adjustments().tone_only();
        let Some(stored) = self.presets.add(name, tone, Vec::new()) else {
            return;
        };
        self.set_status((crate::i18n::t().saved_preset)(&stored));
        self.request_redraw();
    }

    /// Applies one preset to the photo on screen. The selection-wide apply is a
    /// bulk action, because it overwrites N photos with no undo.
    pub(super) fn apply_preset(&mut self, id: u64) {
        let Some(preset) = self.presets.get(id) else {
            return;
        };
        let name = preset.name.clone();
        let tone = preset.adjustments.tone_only();
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let n = self.apply_tone_to(tone, &[path]);
        self.set_status((crate::i18n::t().applied_preset)(&name, n));
    }

    pub(super) fn delete_preset(&mut self, id: u64) {
        let Some(name) = self.presets.get(id).map(|p| p.name.clone()) else {
            return;
        };
        self.presets.remove(id);
        self.set_status((crate::i18n::t().deleted_preset)(&name));
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::develop::Crop;
    use crate::navigation::Playlist;

    const CROP: Crop = Crop {
        left: 0.1,
        top: 0.1,
        right: 0.9,
        bottom: 0.9,
    };

    /// A real `App` over a temp folder of empty files, the shape
    /// `app::keys::tests::folder_app` uses. No window and no GPU.
    fn folder_app(tag: &str, photos: usize) -> (App, PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("lp-presets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..photos)
            .map(|i| {
                let p = dir.join(format!("{i}.jpg"));
                std::fs::write(&p, []).unwrap();
                p
            })
            .collect();
        let mut app = App::new(None);
        app.catalog.open_dir(&dir);
        app.playlist = Some(Playlist::from_dir(&dir));
        app.mode = ViewMode::Grid;
        app.recompute_visible();
        (app, dir, paths)
    }

    fn tone(exposure: f32, contrast: f32) -> Adjustments {
        Adjustments {
            exposure,
            contrast,
            ..Default::default()
        }
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
    fn applying_a_preset_writes_the_shown_photo_and_its_sidecar() {
        let (mut app, dir, paths) = folder_app("shown", 2);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.sel = Some(0);
        let id = {
            app.presets.add("Golden", tone(0.5, 10.0), Vec::new());
            app.presets.presets()[0].id
        };

        app.apply_preset(id);

        assert_eq!(app.edits.get(&paths[0]), Some(&tone(0.5, 10.0)));
        assert_eq!(
            Catalog::with_dir(dir.clone()).adjustments(&paths[0]),
            tone(0.5, 10.0),
            "the sidecar on disk carries the new values"
        );
        assert!(
            app.edits.get(&paths[1]).is_none(),
            "only the photo on screen changed"
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
        app.catalog.set_adjustments(&paths[1], &cropped);
        app.selected = (0..3).collect();
        app.copied_settings = Some((paths[0].clone(), tone(0.25, 5.0)));

        app.apply_settings_to_selection();

        let catalog = Catalog::with_dir(dir.clone());
        for path in &paths {
            let edit = app.edits.get(path).copied().unwrap();
            assert_eq!(edit.exposure, 0.25, "{} got the look", path.display());
            assert_eq!(edit.contrast, 5.0);
            assert_eq!(catalog.adjustments(path).exposure, 0.25);
        }
        assert_eq!(
            app.edits.get(&paths[1]).unwrap().crop,
            Some(CROP),
            "the cropped photo keeps its own crop"
        );
        assert_eq!(app.edits.get(&paths[0]).unwrap().crop, None);
        assert_eq!(catalog.adjustments(&paths[1]).crop, Some(CROP));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_preset_that_merges_to_identity_removes_the_edit_entirely() {
        let (mut app, dir, paths) = folder_app("identity", 1);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.apply_adjustments(tone(0.7, 20.0));
        assert!(app.edits.contains_key(&paths[0]));

        app.presets
            .add("Neutral", Adjustments::default(), Vec::new());
        let id = app.presets.presets()[0].id;
        app.apply_preset(id);

        assert!(
            app.edits.get(&paths[0]).is_none(),
            "an identity merge removes the entry rather than storing a no-op"
        );
        assert!(
            Catalog::with_dir(dir.clone())
                .adjustments(&paths[0])
                .is_identity(),
            "and the sidecar no longer holds the old edit"
        );
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
