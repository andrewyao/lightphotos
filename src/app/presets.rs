use super::*;

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
        self.set_status((crate::i18n::t().renamed_preset)(&stored));
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

    /// Applies one preset across the grid selection. This is a bulk action, so
    /// it is confirmed first: it overwrites eleven fields on N photos with no
    /// undo, and N can be a whole folder after a stray `Cmd+A`.
    pub(super) fn apply_preset_to_selection(&mut self, id: u64) {
        let Some(preset) = self.presets.get(id) else {
            return;
        };
        let name = preset.name.clone();
        let tone = preset.adjustments.tone_only();
        let n = self.apply_tone_to(tone, &self.selected_paths());
        if n == 0 {
            return;
        }
        self.set_status((crate::i18n::t().applied_preset)(&name, n));
    }

    /// Opens the Lightroom preset picker and imports what the user chose.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn import_lr_presets(&mut self) {
        let paths = crate::dialog::pick_xmp_files();
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
                .and_then(|xmp| crate::lr_preset::parse(&xmp));
            let imported = match imported {
                Ok(imported) => imported,
                Err(reason) => {
                    let file = path.file_name().unwrap_or_default().to_string_lossy();
                    failures.push((t.lr_import_failed)(&file, &reason));
                    continue;
                }
            };
            let name = imported.name.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into()
            });
            let mut notes = Vec::new();
            if !imported.dropped.is_empty() {
                notes.push((t.preset_import_note)(&imported.dropped.join(", ")));
            }
            if imported.monochrome {
                notes.push(t.preset_monochrome_note.to_string());
            }
            looks.push((name, imported.adj, notes));
        }

        let stored = self.presets.add_all(looks);
        // One status line, built once. Two `set_status` calls cannot both
        // survive, because it replaces the message rather than appending. What
        // each preset could not carry lives on the row as hover text, which is
        // the durable channel for it; this line lasts three seconds.
        self.set_status((t.lr_import_status)(
            stored.len(),
            failures.len(),
            failures.first().map_or("", String::as_str),
        ));
        self.request_redraw();
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

    /// The shape Camera Raw writes: attributes on the tag, the name in an
    /// `rdf:Alt`, and one field this app has no slider for.
    const NAMED_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\" \
        xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
        crs:Exposure2012=\"+0.35\" crs:Contrast2012=\"+15\" crs:Clarity2012=\"+22\">\
        <crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">Warm Film</rdf:li></rdf:Alt></crs:Name>\
        </rdf:Description></rdf:RDF></x:xmpmeta>";

    /// A preset with no `crs:Name` of its own, in Lightroom's B&W mode.
    const GRAYSCALE_PRESET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
        <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
        <rdf:Description rdf:about=\"\" \
        xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
        crs:Exposure2012=\"-0.2\" crs:ConvertToGrayscale=\"True\"/>\
        </rdf:RDF></x:xmpmeta>";

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

    #[test]
    fn the_selection_wide_apply_is_confirmed_and_then_writes_every_photo() {
        let (mut app, dir, paths) = folder_app("bulk", 3);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        let id = app.presets.presets()[0].id;
        app.selected = (0..3).collect();

        app.request_bulk(crate::ui::BulkKind::ApplyPreset(id));
        let prompt = app.pending_bulk_prompt().expect("the confirm modal opens");
        assert!(
            prompt.contains("Golden") && prompt.contains('3'),
            "the prompt names the preset and the photo count: {prompt}"
        );
        for path in &paths {
            assert!(
                app.edits.get(path).is_none(),
                "nothing is written before the confirmation"
            );
        }

        app.run_bulk(crate::ui::BulkKind::ApplyPreset(id));

        let catalog = Catalog::with_dir(dir.clone());
        for path in &paths {
            assert_eq!(app.edits.get(path).unwrap().exposure, 0.4);
            assert_eq!(catalog.adjustments(path).exposure, 0.4);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One real frame of the whole UI. Returns the actions it pushed and every
    /// string it painted with where it landed, so a test can aim a click at a
    /// widget it cannot see.
    fn frame(app: &mut App, events: Vec<egui::Event>) -> (Vec<crate::ui::UiAction>, Painted) {
        let ctx = app.egui_ctx.clone();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1100.0, 800.0),
            )),
            events,
            ..Default::default()
        };
        let mut actions = Vec::new();
        let output = ctx.run_ui(input, |ui| {
            actions = crate::ui::draw(ui, app).actions;
        });
        let painted = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    text.pos + egui::vec2(4.0, text.galley.size().y / 2.0),
                )),
                _ => None,
            })
            .collect();
        (actions, Painted(painted))
    }

    struct Painted(Vec<(String, egui::Pos2)>);

    impl Painted {
        fn has(&self, text: &str) -> bool {
            self.0.iter().any(|(t, _)| t == text)
        }

        fn any_containing(&self, needle: &str) -> bool {
            self.0.iter().any(|(t, _)| t.contains(needle))
        }

        fn pos_of(&self, text: &str) -> egui::Pos2 {
            self.0
                .iter()
                .find(|(t, _)| t == text)
                .unwrap_or_else(|| panic!("nothing painted {text:?}; got {:?}", self.texts()))
                .1
        }

        /// The `text` nearest `anchor`, for a label the window paints more than
        /// once. Touch Up has its own Delete button, so the row menu's has to be
        /// picked by where it opened.
        fn pos_of_near(&self, text: &str, anchor: egui::Pos2) -> egui::Pos2 {
            self.0
                .iter()
                .filter(|(t, _)| t == text)
                .min_by(|(_, a), (_, b)| a.distance(anchor).total_cmp(&b.distance(anchor)))
                .unwrap_or_else(|| panic!("nothing painted {text:?}; got {:?}", self.texts()))
                .1
        }

        fn texts(&self) -> Vec<&str> {
            self.0.iter().map(|(t, _)| t.as_str()).collect()
        }
    }

    /// What the UI paints once it has settled. A modal is an `egui::Area`,
    /// which egui sizes on one frame and paints on the next, so one frame is
    /// not enough to see one.
    fn settled(app: &mut App) -> Painted {
        let _ = frame(app, Vec::new());
        frame(app, Vec::new()).1
    }

    /// Presses at `pos` in one frame and releases in the next, which is when
    /// egui reports the click. Returns that frame's actions, and what the UI
    /// paints once it has settled afterwards.
    fn click(app: &mut App, pos: egui::Pos2) -> (Vec<crate::ui::UiAction>, Painted) {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        let _ = frame(app, vec![egui::Event::PointerMoved(pos), button(true)]);
        let (actions, _) = frame(app, vec![button(false)]);
        (actions, settled(app))
    }

    /// The pointer paths the tests above reach through `App` directly: clicking
    /// a preset row, and opening its row menu. Driven as real clicks against the
    /// real widget tree, because a wiring mistake between the widget and the
    /// action would not show up anywhere else.
    #[test]
    fn clicking_a_preset_row_applies_it_to_the_shown_photo() {
        let (mut app, dir, paths) = folder_app("click", 2);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        app.presets.add("Moody", tone(-0.4, 0.0), Vec::new());
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.develop_open = true;

        let painted = settled(&mut app);
        assert!(
            painted.has(crate::i18n::t().presets),
            "the Develop panel has a Presets section: {:?}",
            painted.texts()
        );
        assert!(
            !painted.has("Golden"),
            "which starts collapsed, so no slider moves"
        );

        let (_, painted) = click(&mut app, painted.pos_of(crate::i18n::t().presets));
        assert!(
            painted.has("Golden") && painted.has("Moody"),
            "clicking the header reveals every row: {:?}",
            painted.texts()
        );

        let (actions, _) = click(&mut app, painted.pos_of("Golden"));
        let golden = app
            .presets
            .presets()
            .iter()
            .find(|p| p.name == "Golden")
            .unwrap()
            .id;
        assert!(
            actions.contains(&crate::ui::UiAction::ApplyPreset(golden)),
            "the row click asks to apply that preset: {actions:?}"
        );

        app.apply_ui_actions(actions);
        assert_eq!(
            app.edits.get(&paths[0]),
            Some(&tone(0.4, 0.0)),
            "and the look lands on the photo on screen"
        );
        assert!(
            app.edits.get(&paths[1]).is_none(),
            "only the photo on screen changed"
        );

        let dots = painted.pos_of("\u{22ef}");
        let (actions, menu) = click(&mut app, dots);
        assert!(actions.is_empty(), "the row menu opens without acting");
        let t = crate::i18n::t();
        assert!(
            menu.has(t.rename) && menu.has(t.delete),
            "and offers rename and delete: {:?}",
            menu.texts()
        );

        let (actions, _) = click(&mut app, menu.pos_of_near(t.delete, dots));
        assert!(
            actions.contains(&crate::ui::UiAction::RequestDeletePreset(golden)),
            "whose Delete asks for confirmation rather than deleting: {actions:?}"
        );
        assert_eq!(app.presets.presets().len(), 2, "nothing deleted yet");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grid's own entry point, which the Develop panel cannot reach: the
    /// panel only draws in the Loupe and the selection bar only outside it.
    #[test]
    fn the_selection_bar_dropdown_asks_to_apply_across_the_selection() {
        let (mut app, dir, _) = folder_app("dropdown", 3);
        app.presets.add("Golden", tone(0.4, 0.0), Vec::new());
        app.mode = ViewMode::Grid;
        app.selected = (0..3).collect();

        let t = crate::i18n::t();
        let painted = settled(&mut app);
        assert!(
            painted.has(t.preset_menu),
            "the selection bar offers the preset dropdown: {:?}",
            painted.texts()
        );

        let (_, open) = click(&mut app, painted.pos_of(t.preset_menu));
        assert!(
            open.has("Golden"),
            "which lists every preset: {:?}",
            open.texts()
        );

        let golden = app.presets.presets()[0].id;
        let (actions, _) = click(&mut app, open.pos_of("Golden"));
        assert!(
            actions.contains(&crate::ui::UiAction::RequestBulk(
                crate::ui::BulkKind::ApplyPreset(golden)
            )),
            "and picking one requests the confirmed bulk action: {actions:?}"
        );

        app.apply_ui_actions(actions);
        assert!(
            app.pending_bulk_prompt().is_some(),
            "so the confirmation opens before anything is written"
        );
        assert!(app.edits.is_empty(), "nothing written before the confirm");

        let _ = std::fs::remove_dir_all(&dir);
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
        app.pending_preset_delete = Some(app.presets.presets()[0].id);
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
            [(t.preset_import_note)("Clarity2012")],
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

    /// The Import entry's own wiring. `import_lr_presets` opens a blocking OS
    /// dialog, so the action the button pushes is as far as a test can drive it,
    /// and a button wired to the wrong action would show up nowhere else.
    #[test]
    fn the_import_button_asks_to_open_the_lightroom_picker() {
        let (mut app, dir, paths) = folder_app("lr-button", 1);
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.develop_open = true;

        let t = crate::i18n::t();
        let painted = settled(&mut app);
        let (_, open) = click(&mut app, painted.pos_of(t.presets));
        assert!(
            open.has(t.import_lr_presets),
            "the Presets block offers the import entry: {:?}",
            open.texts()
        );

        let (actions, _) = click(&mut app, open.pos_of(t.import_lr_presets));
        assert!(
            actions.contains(&crate::ui::UiAction::ImportLrPresets),
            "which asks App to open the picker: {actions:?}"
        );

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
