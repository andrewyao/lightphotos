use super::*;

use winit::keyboard::KeyCode;

use crate::ui;

impl App {
    /// Whether an arrow or Tab key that egui marked "consumed" should still reach
    /// app navigation. egui keeps focus on a slider after a drag and consumes
    /// every later arrow key, and egui_winit consumes every Tab press. We ignore
    /// that unless a slider is being dragged right now, a crop is in progress,
    /// or a text field holds focus, where Left has to move the caret rather
    /// than also stepping to the previous photo. `text_edit_focused` rather
    /// than `egui_wants_keyboard_input`, which is true of any focused widget
    /// and so would strand the arrows this whole method exists to let through.
    pub(crate) fn nav_key_should_fall_through(&self) -> bool {
        self.crop_edit.is_none()
            && !self.egui_ctx.egui_is_using_pointer()
            && !self.egui_ctx.text_edit_focused()
    }

    /// Handle a key press per the Lightroom key-binding table.
    pub(crate) fn handle_key(&mut self, code: KeyCode) {
        let shift = self.modifiers.shift_key();
        // Either modifier works as the accelerator everywhere. Only Cmd is
        // idiomatic on a Mac and only Ctrl on Windows and Linux, but a browser
        // window can be either, so both are accepted and no binding here means
        // anything different under one of them. macOS also reads Ctrl+A/E/K as
        // emacs caret motions, but those only apply inside a text field, and
        // egui consumes every key while one has focus, so `main.rs` returns
        // before this runs.
        let cmd = self.modifiers.super_key() || self.modifiers.control_key();
        let alt = self.modifiers.alt_key();

        // Alt+= and Alt+- size all text, in any mode. Shift is allowed so `+` works.
        if alt && !cmd {
            let steps = match code {
                KeyCode::Equal | KeyCode::NumpadAdd => 1,
                KeyCode::Minus | KeyCode::NumpadSubtract => -1,
                _ => 0,
            };
            if steps != 0 {
                ui::font_size::step(&self.egui_ctx, steps);
                self.request_redraw();
                return;
            }
        }

        // While cropping, other keys do nothing, so a stray arrow or digit
        // can't move the selection out from under the crop.
        if self.crop_edit.is_some() {
            match code {
                KeyCode::KeyC | KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.cancel_crop(),
                KeyCode::KeyX if !cmd && !alt => self.toggle_export_form(),
                _ => {}
            }
            return;
        }

        // While the WB picker is armed, only Escape (cancel) does anything.
        if self.tool == LoupeTool::WbPicker {
            if code == KeyCode::Escape {
                self.tool = LoupeTool::None;
                self.request_redraw();
            }
            return;
        }

        if self.tool == LoupeTool::TouchUp {
            match code {
                KeyCode::Escape => {
                    self.tool = LoupeTool::None;
                    self.touchup_selected = None;
                    self.request_redraw();
                }
                KeyCode::Delete | KeyCode::Backspace => self.delete_selected_touchup(),
                KeyCode::KeyZ if cmd => {
                    if self.touchup_selected.is_none() && !self.current_touchups().is_empty() {
                        self.touchup_selected = Some(self.current_touchups().len() - 1);
                    }
                    self.delete_selected_touchup();
                }
                _ => {}
            }
            return;
        }

        // While the name prompt is open, only Escape and Enter act. egui
        // swallows keys while the field holds focus, so this is the fallback
        // for a prompt that lost focus, where `x` would export and a digit
        // would re-rate.
        if self.preset_name_edit.is_some() {
            match code {
                KeyCode::Escape => self.cancel_preset_name(),
                KeyCode::Enter | KeyCode::NumpadEnter => self.commit_preset_name(),
                _ => {}
            }
            return;
        }

        // While the export form is open, Enter runs it and Escape closes it.
        // Other keys still act, so the selection can change under the form.
        if self.export_form_open() {
            match code {
                KeyCode::Escape => return self.close_export_form(),
                KeyCode::Enter | KeyCode::NumpadEnter => return self.run_export_form(),
                _ => {}
            }
        }

        // `?` (Shift+/) toggles the shortcut help.
        if shift && code == KeyCode::Slash {
            self.show_help = !self.show_help;
            self.request_redraw();
            return;
        }
        if self.show_help && code == KeyCode::Escape {
            self.show_help = false;
            self.request_redraw();
            return;
        }

        // In the quit prompt, Escape confirms and Enter cancels. Repeated Escape
        // backs out of the app one step at a time, and Enter always goes deeper in.
        if self.pending_quit {
            match code {
                KeyCode::Escape => self.quit_requested = true,
                KeyCode::Enter | KeyCode::NumpadEnter => {
                    self.pending_quit = false;
                    self.request_redraw();
                }
                _ => {}
            }
            return;
        }

        // In Survey, Left/Right pick which photo the rating keys apply to.
        // Digits fall through to the shared rating code below.
        if self.mode == ViewMode::Survey {
            match code {
                KeyCode::Escape => {
                    self.close_survey();
                    return;
                }
                KeyCode::ArrowLeft => {
                    self.survey_move_focus(-1);
                    return;
                }
                KeyCode::ArrowRight => {
                    self.survey_move_focus(1);
                    return;
                }
                KeyCode::Enter | KeyCode::NumpadEnter if !cmd => {
                    self.keep_best_reject_rest();
                    return;
                }
                _ => {}
            }
        }

        // Plain digits set the rating, and 0 clears it.
        if !shift && !cmd && !alt {
            if let Some(n) = digit_of(code) {
                self.set_rating(n);
                return;
            }
        }

        let loupe = self.mode == ViewMode::Loupe;
        match code {
            // Shift is allowed so `)`, `!` and `+` work on a US layout.
            KeyCode::Digit0 | KeyCode::Numpad0 if cmd && loupe => self.fit_to_window(),
            KeyCode::Digit1 | KeyCode::Numpad1 if cmd && loupe => self.reset_100(),
            KeyCode::Equal | KeyCode::NumpadAdd if cmd && loupe => self.zoom_by(1.2),
            KeyCode::Minus | KeyCode::NumpadSubtract if cmd && loupe => self.zoom_by(1.0 / 1.2),
            KeyCode::BracketLeft if loupe => self.rotate(false),
            KeyCode::BracketRight if loupe => self.rotate(true),
            // `main.rs` sends Space only on a tap, not while it's held to pan.
            KeyCode::Space if loupe => self.cycle_zoom(),
            KeyCode::Space if self.focus == Region::Grid => self.nav_enter(),

            // F6 cycles between the main region and the Toolbar and Filmstrip.
            // Inside an entered region it moves between that region's controls.
            KeyCode::F6 => match self.focus_level {
                FocusLevel::Selected => self.cycle_region(shift),
                FocusLevel::Entered => self.cycle_control(shift),
            },
            // Tab and Shift+Tab move between items inside the focused region.
            // F6 moves between regions.
            KeyCode::Tab if self.focus == Region::Folders => {
                self.focus_level = FocusLevel::Entered;
                self.folder_move(if shift { -1 } else { 1 });
            }
            KeyCode::Tab if self.focus == Region::Grid && self.mode == ViewMode::Grid => {
                self.focus_level = FocusLevel::Entered;
                self.move_grid(if shift { -1 } else { 1 }, 0);
            }
            KeyCode::Tab if self.focus == Region::Toolbar => {
                self.focus_level = FocusLevel::Entered;
                self.toolbar_move(if shift { -1 } else { 1 });
            }
            KeyCode::Tab if self.focus == Region::Develop => {
                self.focus_level = FocusLevel::Entered;
                self.develop_move(if shift { -1 } else { 1 });
            }
            KeyCode::Tab if self.focus == Region::Filmstrip => {
                self.focus_level = FocusLevel::Entered;
                self.step_loupe(!shift);
            }

            KeyCode::KeyO if cmd && !alt => self.open_folder_picker(),

            KeyCode::KeyG => self.enter_grid(),
            KeyCode::KeyI
                if !cmd && !alt && matches!(self.mode, ViewMode::Grid | ViewMode::Loupe) =>
            {
                self.toggle_left_tab()
            }
            // Gated with the toolbar buttons, so the grouping tools are either
            // wholly on or wholly off. Their two thresholds are still guesses
            // that no real photo set has checked, and a key that silently
            // applies an unvalidated verdict is worse than no key.
            KeyCode::KeyB if !cmd && !alt && crate::app::SHOW_GROUPING_TOOLS => {
                self.toggle_bursts()
            }
            KeyCode::KeyD if !cmd && !alt && crate::app::SHOW_GROUPING_TOOLS => self.toggle_dupes(),
            KeyCode::KeyE => {
                if self.mode == ViewMode::Grid {
                    self.enter_loupe();
                }
            }
            KeyCode::KeyC if !cmd && !alt => self.enter_crop(),
            KeyCode::KeyX if !cmd && !alt => self.toggle_export_form(),
            KeyCode::Enter | KeyCode::NumpadEnter => self.nav_enter(),
            // Cmd+Shift+U tones the selection. It must come before plain Cmd+U.
            KeyCode::KeyU if cmd && shift => self.request_bulk(ui::BulkKind::AutoTone),
            KeyCode::KeyU if cmd => self.auto_tone_one(),
            // Cmd+Shift+Y pastes copied settings onto the selection. It must
            // come before plain `Y`, which toggles the before/after view.
            KeyCode::KeyY if cmd && shift && self.has_copied_settings() => {
                self.request_bulk(ui::BulkKind::ApplySettings)
            }
            KeyCode::KeyY if self.mode == ViewMode::Loupe && !cmd => self.toggle_compare(),
            // Escape undoes Enter one step per press, ending at the quit
            // prompt. Leaving the Grid for Folders also clears the selection.
            KeyCode::Escape => {
                if CHROME_ORDER.contains(&self.focus) {
                    self.focus = self.main_focus;
                    self.focus_level = FocusLevel::Selected;
                    self.on_focus_changed();
                } else if self.focus == Region::Develop {
                    self.focus = Region::Detail;
                    self.focus_level = FocusLevel::Selected;
                    self.on_focus_changed();
                } else if self.focus == Region::Detail {
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.normalize_focus();
                } else if self.focus == Region::Grid
                    && self.focus_level == FocusLevel::Selected
                    && self.region_available(Region::Folders)
                {
                    self.focus = Region::Folders;
                    self.focus_level = FocusLevel::Selected;
                    self.on_focus_changed();
                    self.sel = None;
                    self.selected.clear();
                    self.anchor = None;
                } else if self.focus_level == FocusLevel::Entered {
                    self.focus_level = FocusLevel::Selected;
                } else {
                    // A browser tab can't quit, so the web build does nothing.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        self.pending_quit = true;
                    }
                }
                self.request_redraw();
            }

            KeyCode::KeyA if cmd && self.mode == ViewMode::Grid => self.select_all(),
            KeyCode::KeyC if cmd && shift => self.copy_settings(),
            KeyCode::KeyP if cmd && shift => self.prompt_save_preset(),
            KeyCode::Delete => self.request_bulk(ui::BulkKind::Delete),

            KeyCode::ArrowLeft => self.nav_arrow(-1, 0, shift),
            KeyCode::ArrowRight => self.nav_arrow(1, 0, shift),
            KeyCode::ArrowUp => self.nav_arrow(0, -1, shift),
            KeyCode::ArrowDown => self.nav_arrow(0, 1, shift),

            KeyCode::PageUp if matches!(self.focus, Region::Detail | Region::Develop) => {
                self.step_loupe(false)
            }
            KeyCode::PageDown if matches!(self.focus, Region::Detail | Region::Develop) => {
                self.step_loupe(true)
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::Playlist;
    use std::sync::atomic::{AtomicU64, Ordering};
    use winit::keyboard::ModifiersState;

    #[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
    const CMD: ModifiersState = ModifiersState::SUPER;
    #[cfg(not(all(not(target_arch = "wasm32"), target_os = "macos")))]
    const CMD: ModifiersState = ModifiersState::CONTROL;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn folder_app(photos: usize) -> (App, Vec<PathBuf>) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-keys-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = (0..photos)
            .map(|i| {
                let p = dir.join(format!("{i}.jpg"));
                std::fs::write(&p, b"").unwrap();
                p
            })
            .collect();
        let mut app = App::new(None);
        app.catalog.open_dir(&dir);
        app.playlist = Some(Playlist::from_dir(&dir));
        app.mode = ViewMode::Grid;
        app.focus = Region::Grid;
        app.recompute_visible();
        app.sel = Some(0);
        (app, paths)
    }

    /// An editor showing a 1000x1000 photo in a 500x500 viewport, so fit is 0.5.
    fn editor_app() -> (App, Vec<PathBuf>) {
        let (mut app, paths) = folder_app(2);
        app.mode = ViewMode::Loupe;
        app.focus = Region::Detail;
        app.want = Some(paths[0].clone());
        app.shown = Shown::Preview(paths[0].clone(), 1000, 1000);
        app.source_size = Some((1000, 1000));
        app.loupe_viewport = Some((0, 0, 500, 500));
        app.fit_to_window();
        (app, paths)
    }

    fn press(app: &mut App, mods: ModifiersState, code: KeyCode) {
        app.modifiers = mods;
        app.handle_key(code);
    }

    fn assert_zoom(app: &App, want: f32) {
        assert!(
            (app.zoom() - want).abs() < 1e-4,
            "zoom {} != {want}",
            app.zoom()
        );
    }

    #[test]
    fn space_opens_the_selected_photo_from_the_library() {
        let (mut app, _) = folder_app(2);
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert_eq!(app.mode, ViewMode::Loupe);
    }

    #[test]
    fn left_and_right_step_photos_in_the_editor() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowRight);
        assert_eq!(app.sel, Some(1));
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowLeft);
        assert_eq!(app.sel, Some(0));
    }

    #[test]
    fn up_and_down_zoom_by_ten_percent_in_the_editor() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowUp);
        assert_zoom(&app, 0.55);
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowDown);
        assert_zoom(&app, 0.5);
        assert_eq!(app.sel, Some(0));
    }

    #[test]
    fn space_cycles_fit_then_double_fit_then_one_to_one() {
        let (mut app, _) = editor_app();
        app.source_size = Some((4000, 4000));
        app.fit_to_window();
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert_zoom(&app, 0.25);
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert_zoom(&app, 1.0);
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert!(app.fitted);
        assert_zoom(&app, 0.125);
    }

    #[test]
    fn space_skips_a_stage_that_would_not_change_the_zoom() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert_zoom(&app, 1.0);
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert!(app.fitted, "2x fit is already 100%, so the next press fits");
    }

    #[test]
    fn primary_modifier_zoom_keys() {
        let (mut app, _) = editor_app();
        press(&mut app, CMD, KeyCode::Digit1);
        assert_zoom(&app, 1.0);
        press(&mut app, CMD, KeyCode::Digit0);
        assert!(app.fitted);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::Digit1);
        assert_zoom(&app, 1.0);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::Digit0);
        assert!(app.fitted);

        press(&mut app, CMD, KeyCode::Equal);
        assert_zoom(&app, 0.6);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::Equal);
        assert_zoom(&app, 0.72);
        press(&mut app, CMD, KeyCode::Minus);
        assert_zoom(&app, 0.6);
        assert!(app.selected_label().is_none(), "zoom keys must not label");
        assert_eq!(app.rating_of(&app.selected_path().unwrap()), 0);
    }

    #[test]
    fn brackets_rotate_without_a_modifier() {
        let (mut app, paths) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::BracketRight);
        assert_eq!(app.rotations.get(&paths[0]), Some(&1));
        press(&mut app, ModifiersState::empty(), KeyCode::BracketLeft);
        press(&mut app, ModifiersState::empty(), KeyCode::BracketLeft);
        assert_eq!(app.rotations.get(&paths[0]), Some(&3));
    }

    /// The grouping tools are either wholly reachable or wholly absent. While
    /// the flag is off the keys must do nothing, and the shortcut overlay must
    /// not advertise them, or a user presses a documented key and sees no
    /// change. Flip `SHOW_GROUPING_TOOLS` and this test follows it.
    #[test]
    fn the_grouping_keys_and_their_help_agree_with_the_flag() {
        let (mut app, _paths) = folder_app(2);
        let before = (app.bursts_on(), app.dupes_on());

        press(&mut app, ModifiersState::empty(), KeyCode::KeyB);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyD);

        if crate::app::SHOW_GROUPING_TOOLS {
            assert_ne!(
                (app.bursts_on(), app.dupes_on()),
                before,
                "B and D must toggle while the grouping tools are on"
            );
        } else {
            assert_eq!(
                (app.bursts_on(), app.dupes_on()),
                before,
                "B and D must do nothing while the grouping tools are off"
            );
        }

        let advertised = crate::i18n::t()
            .help
            .iter()
            .flat_map(|s| s.rows)
            .any(|(keys, _)| *keys == "B" || *keys == "D");
        assert!(
            !advertised,
            "the grouping tools are unfinished, so the overlay must not list B or D"
        );
    }

    /// Color labels have no UI yet, so the keys that used to set them are
    /// gone and the overlay does not mention them.
    #[test]
    fn shift_digits_do_nothing() {
        let (mut app, _) = folder_app(1);
        press(&mut app, ModifiersState::SHIFT, KeyCode::Digit2);
        assert_eq!(app.selected_label(), None);
        assert_eq!(app.selected_rating(), 0, "Shift+digit is not a rating");
        assert!(app.filter.is_none(), "Shift+digit does not filter");

        let advertised = crate::i18n::t()
            .help
            .iter()
            .flat_map(|s| s.rows)
            .any(|(keys, _)| keys.starts_with("Shift+0") || keys.starts_with("Shift+1"));
        assert!(!advertised, "the overlay must not list the color labels");
    }

    #[test]
    fn arrows_stay_out_of_navigation_while_a_text_field_has_focus() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowRight);
        assert_eq!(app.sel, Some(1));
        assert!(
            app.nav_key_should_fall_through(),
            "with nothing focused, an arrow egui consumed still navigates"
        );

        // The first frame asks for focus; egui reports it from the second on.
        let mut text = String::from("Golden");
        for _ in 0..2 {
            let _ = app.egui_ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.add(egui::TextEdit::singleline(&mut text))
                    .request_focus();
            });
        }
        assert!(
            !app.nav_key_should_fall_through(),
            "Left must move the caret, not step to the previous photo"
        );

        // What `main.rs` does with the guard for a key egui consumed.
        if app.nav_key_should_fall_through() {
            app.handle_key(KeyCode::ArrowLeft);
        }
        assert_eq!(app.sel, Some(1), "the shown photo did not change");
    }

    #[test]
    fn the_save_preset_shortcut_opens_the_name_prompt() {
        let (mut app, _) = editor_app();
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::KeyP);
        assert!(
            app.preset_name_edit().is_some(),
            "Cmd+Shift+P opens the prompt"
        );

        // The guard: while the prompt is open a stray key must not export or
        // re-rate behind it.
        press(&mut app, ModifiersState::empty(), KeyCode::KeyX);
        press(&mut app, ModifiersState::empty(), KeyCode::Digit3);
        assert!(app.preset_name_edit().is_some(), "the prompt is still open");

        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert!(app.preset_name_edit().is_none(), "Escape closes it");
    }

    /// Escape and Enter belong to the form while it is open. Without the
    /// guard, Escape would back the Grid out to Folders and drop the selection
    /// being exported, and Enter would open the Loupe.
    #[test]
    fn x_opens_the_export_form_and_it_owns_escape_and_enter() {
        let (mut app, _) = folder_app(2);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyX);
        assert!(app.export_form_open(), "X opens the form");

        press(&mut app, ModifiersState::empty(), KeyCode::Enter);
        assert_eq!(app.mode, ViewMode::Grid, "Enter did not open the Loupe");

        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert!(!app.export_form_open(), "Escape closes the form");
        assert_eq!(app.focus, Region::Grid, "and only the form");
        assert_eq!(app.sel, Some(0), "the selection survives");

        press(&mut app, ModifiersState::empty(), KeyCode::KeyX);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyX);
        assert!(!app.export_form_open(), "X again closes it");
    }

    #[test]
    fn a_focused_slider_still_lets_arrows_navigate() {
        let (app, _) = editor_app();
        let mut v = 0.5f32;
        for _ in 0..2 {
            let _ = app.egui_ctx.run_ui(egui::RawInput::default(), |ui| {
                ui.add(egui::Slider::new(&mut v, 0.0..=1.0)).request_focus();
            });
        }
        assert!(
            app.nav_key_should_fall_through(),
            "egui parks focus on a slider after a drag; arrows must still step photos"
        );
    }

    #[test]
    fn modified_scroll_pans_or_zooms() {
        let (mut app, _) = editor_app();
        press(&mut app, CMD, KeyCode::Digit1);
        let pan = app.pan;

        app.modifiers = ModifiersState::SHIFT;
        app.on_scroll(0.0, 30.0);
        assert_eq!(
            app.pan,
            (pan.0 + 30.0, pan.1),
            "Shift+scroll pans horizontally"
        );
        assert_zoom(&app, 1.0);

        app.modifiers = ModifiersState::ALT;
        app.on_scroll(0.0, 30.0);
        assert_eq!(
            app.pan,
            (pan.0 + 30.0, pan.1 + 30.0),
            "Alt+scroll pans vertically"
        );

        app.modifiers = ModifiersState::SHIFT | ModifiersState::ALT;
        app.on_scroll(0.0, 30.0);
        assert!(app.zoom() > 1.0, "Shift+Alt+scroll zooms");
    }

    #[test]
    fn i_toggles_the_left_panel_tab_in_the_library_and_the_editor() {
        let (mut app, _) = folder_app(2);
        assert_eq!(app.left_tab(), LeftTab::Folders);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.left_tab(), LeftTab::Info);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.left_tab(), LeftTab::Folders);

        press(&mut app, CMD, KeyCode::KeyI);
        press(&mut app, ModifiersState::ALT, KeyCode::KeyI);
        assert_eq!(
            app.left_tab(),
            LeftTab::Folders,
            "modified I is not the toggle"
        );

        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.left_tab(), LeftTab::Info);
    }

    #[test]
    fn the_grid_reads_metadata_only_while_the_info_tab_shows_it() {
        let (mut app, paths) = folder_app(2);
        assert_eq!(
            app.metadata_to_read(),
            None,
            "the folder tree shows no metadata"
        );
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.metadata_to_read(), Some(paths[0].clone()));

        app.on_exif_info(vec![(paths[0].clone(), Default::default())]);
        assert_eq!(
            app.metadata_to_read(),
            None,
            "a cached photo is not read again"
        );

        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        app.enter_loupe();
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowRight);
        assert_eq!(
            app.metadata_to_read(),
            Some(paths[1].clone()),
            "the Loupe's info bar reads on either tab"
        );
    }

    #[test]
    fn the_info_tab_takes_the_folder_tree_out_of_keyboard_focus() {
        let (mut app, _) = folder_app(2);
        app.set_focus(Region::Folders, FocusLevel::Selected);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.focus, Region::Grid, "focus leaves the hidden tree");

        for _ in 0..4 {
            press(&mut app, ModifiersState::empty(), KeyCode::F6);
            assert_ne!(
                app.focus,
                Region::Folders,
                "F6 never lands on the hidden tree"
            );
        }

        app.set_focus(Region::Grid, FocusLevel::Selected);
        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert_ne!(
            app.focus,
            Region::Folders,
            "Escape does not back into the hidden tree"
        );
        assert_eq!(app.sel, Some(0), "and so keeps the selection");
    }

    #[test]
    fn image_overflows_only_when_zoomed_past_the_window() {
        let (mut app, _) = editor_app();
        app.fit_to_window();
        assert!(!app.image_overflows(), "a fitted image has nothing to pan");
        app.zoom_by(3.0);
        assert!(app.image_overflows(), "3x fit is larger than the window");
    }
}
