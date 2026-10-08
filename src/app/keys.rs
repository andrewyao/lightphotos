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
        !self.cropping()
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

        // A confirm dialog sits over every tool, so it owns the keyboard ahead
        // of them. Handling Esc here clears the dialog before its next frame,
        // so the modal's own Esc check never runs and nothing cancels twice.
        if self.confirm_open() {
            match code {
                KeyCode::Escape => self.cancel_pending(),
                KeyCode::Enter | KeyCode::NumpadEnter => self.confirm_pending(),
                KeyCode::Tab => self.step_group_delete_focus(shift),
                _ => {}
            }
            return;
        }

        // While cropping, other keys do nothing, so a stray arrow or digit
        // can't move the selection out from under the crop.
        if self.straighten_tool() != StraightenTool::Off {
            match code {
                KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.toggle_straighten_tool(),
                _ => {}
            }
            return;
        }
        if self.cropping() {
            match code {
                KeyCode::KeyC | KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.cancel_crop(),
                KeyCode::BracketLeft if !cmd && !alt => self.rotate(false),
                KeyCode::BracketRight if !cmd && !alt => self.rotate(true),
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
                KeyCode::KeyZ if cmd => self.undo_touchup(),
                // Brackets size the brush here instead of rotating.
                KeyCode::BracketLeft | KeyCode::BracketRight if !cmd && !alt => {
                    let up = code == KeyCode::BracketRight;
                    if shift {
                        self.step_touchup_feather(if up { 0.1 } else { -0.1 });
                    } else {
                        self.step_touchup_radius(up);
                    }
                }
                KeyCode::KeyQ if !cmd && !alt => self.toggle_touchup(),
                KeyCode::KeyO if !cmd && !alt && shift => self.select_next_touchup(),
                KeyCode::KeyO if !cmd && !alt => self.toggle_touchup_spots(),
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

        // Cmd+, toggles Settings. While it is open, Esc closes it and every
        // other key waits, so a digit can't re-rate the photo behind it.
        if cmd && code == KeyCode::Comma {
            self.show_settings = !self.show_settings;
            self.request_redraw();
            return;
        }
        if self.show_settings {
            if code == KeyCode::Escape {
                self.show_settings = false;
                self.request_redraw();
            }
            return;
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

        // Plain digits 1-5 set the rating, and 0 clears it. 6-9 do nothing.
        if !shift && !cmd && !alt {
            if let Some(n) = digit_of(code).filter(|&n| n <= crate::catalog::MAX_RATING) {
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
            KeyCode::KeyO if loupe && !cmd && !alt => self.selection_key(shift),
            KeyCode::KeyK if loupe && !cmd && !alt => self.set_develop_tab(DevelopTab::Masks),
            KeyCode::KeyQ if loupe && !cmd && !alt => {
                self.set_develop_tab(DevelopTab::Masks);
                self.toggle_touchup();
            }

            KeyCode::KeyG if cmd && alt => self.group_bursts(),
            KeyCode::KeyG if cmd && shift => self.ungroup_selected(),
            KeyCode::KeyG if cmd => self.group_selected(),
            KeyCode::KeyG if !alt => self.enter_grid(),
            KeyCode::KeyI if !cmd && !alt => self.click_rail(RailItem::Info),
            KeyCode::KeyE if !cmd && !alt && self.mode == ViewMode::Grid => self.enter_loupe(),
            KeyCode::KeyC if !cmd && !alt => self.enter_crop(),
            KeyCode::KeyX if !cmd && !alt => self.toggle_export_form(),
            KeyCode::Enter | KeyCode::NumpadEnter => self.nav_enter(),
            // Cmd+Shift+U tones the selection. It must come before plain Cmd+U.
            KeyCode::KeyU if cmd && shift => self.request_bulk(ui::BulkKind::AutoTone),
            KeyCode::KeyU if cmd => self.auto_tone_one(),
            KeyCode::KeyS if cmd && shift => self.score_selection(),
            // Cmd+Shift+Y pastes copied settings onto the selection. It must
            // come before plain `Y`, which toggles the before/after view.
            KeyCode::KeyY if cmd && shift && self.has_copied_settings() => {
                self.request_bulk(ui::BulkKind::ApplySettings)
            }
            KeyCode::KeyY if self.mode == ViewMode::Loupe && !cmd && !alt => self.toggle_compare(),
            // Escape undoes Enter one step per press and stops at the Grid
            // or Folders. Leaving the Grid for Folders also clears the selection.
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
                }
                self.request_redraw();
            }

            KeyCode::KeyA if cmd && self.mode == ViewMode::Grid => self.select_all(),
            KeyCode::KeyC if cmd && shift => self.copy_settings(),
            KeyCode::KeyP if cmd && shift && crate::app::SHOW_PRESETS => self.prompt_save_preset(),
            KeyCode::Delete if shift && self.selection_has_group() => self.request_delete_group(),
            KeyCode::Delete if !self.group_picks().is_empty() => self.request_delete_picks(),
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
    use crate::app::nav::tests::group_photos;
    use crate::navigation::Playlist;
    use std::collections::BTreeSet;
    use winit::keyboard::ModifiersState;

    #[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
    const CMD: ModifiersState = ModifiersState::SUPER;
    #[cfg(not(all(not(target_arch = "wasm32"), target_os = "macos")))]
    const CMD: ModifiersState = ModifiersState::CONTROL;

    fn folder_app(photos: usize) -> (App, Vec<PathBuf>) {
        let (mut app, _, paths) = crate::app::test_support::folder_app("keys-test", photos);
        app.focus = Region::Grid;
        app.sel = Some(0);
        (app, paths)
    }

    /// An editor showing a 1000x1000 photo in a 500x500 viewport, so fit is 0.5.
    fn editor_app() -> (App, Vec<PathBuf>) {
        let (mut app, paths) = folder_app(2);
        app.mode = ViewMode::Loupe;
        app.develop_open = true;
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

    /// Esc used to walk out of the app and end at a quit prompt that a
    /// further Esc confirmed. Now the chain stops at Folders and stays there.
    #[test]
    fn esc_from_a_fresh_grid_never_leaves_the_library() {
        let (mut app, _) = folder_app(2);
        for _ in 0..5 {
            press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        }
        assert_eq!(app.mode, ViewMode::Grid);
        assert_eq!(app.focus, Region::Folders);
        assert_eq!(app.focus_level, FocusLevel::Selected);
    }

    #[test]
    fn an_open_delete_confirm_swallows_keys_until_esc_cancels_it() {
        let (mut app, paths) = folder_app(3);
        press(&mut app, CMD, KeyCode::KeyA);
        press(&mut app, ModifiersState::empty(), KeyCode::Delete);
        assert!(app.confirm_open(), "Delete asks first");
        let painted = crate::app::test_support::settled(&mut app);
        let t = crate::i18n::t();
        assert!(
            painted.has(t.bulk_delete) && !painted.has(t.confirm),
            "the dialog names its action, not a generic Confirm: {:?}",
            painted.texts()
        );

        for code in [KeyCode::Digit2, KeyCode::KeyG, KeyCode::ArrowRight] {
            press(&mut app, ModifiersState::empty(), code);
        }
        assert!(app.confirm_open(), "the dialog is still open");
        assert!(
            paths.iter().all(|p| app.rating_of(p) == 0),
            "2 did not rate the photos behind the dialog"
        );
        assert_eq!(app.sel, Some(0), "the arrow did not move the selection");

        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert!(!app.confirm_open(), "Esc cancels");
        assert_eq!(
            (app.focus, app.selection_count()),
            (Region::Grid, 3),
            "and does nothing else, so focus and the selection stay"
        );
        assert!(paths.iter().all(|p| p.exists()), "nothing was deleted");
    }

    #[test]
    fn a_digit_rates_every_selected_photo_without_asking() {
        let (mut app, paths) = folder_app(3);
        press(&mut app, CMD, KeyCode::KeyA);
        press(&mut app, ModifiersState::empty(), KeyCode::Digit4);
        assert!(!app.confirm_open(), "no confirmation");
        assert!(paths.iter().all(|p| app.rating_of(p) == 4));
        press(&mut app, ModifiersState::empty(), KeyCode::Digit0);
        assert!(
            paths.iter().all(|p| app.rating_of(p) == 0),
            "0 clears them all"
        );
    }

    #[test]
    fn a_digit_with_one_photo_selected_rates_only_that_photo() {
        let (mut app, paths) = folder_app(3);
        press(&mut app, ModifiersState::empty(), KeyCode::Digit3);
        let ratings: Vec<u8> = paths.iter().map(|p| app.rating_of(p)).collect();
        assert_eq!(ratings, [3, 0, 0]);
    }

    #[test]
    fn enter_runs_the_pending_bulk_action() {
        let (mut app, paths) = folder_app(3);
        press(&mut app, CMD, KeyCode::KeyA);
        let look = crate::develop::Adjustments {
            exposure: 0.5,
            ..Default::default()
        };
        app.copied_settings = Some((paths[0].clone(), look));
        app.request_bulk(ui::BulkKind::ApplySettings);
        assert!(app.confirm_open(), "applying a look asks first");
        press(&mut app, ModifiersState::empty(), KeyCode::Enter);
        assert!(!app.confirm_open());
        assert!(
            paths
                .iter()
                .all(|p| app.catalog.adjustments(p).exposure == 0.5),
            "every photo got the look"
        );
        assert_eq!(
            app.mode,
            ViewMode::Grid,
            "Enter did not also open the Loupe"
        );
    }

    #[test]
    fn esc_cancels_a_preset_delete_and_enter_confirms_it() {
        let (mut app, _) = editor_app();
        app.presets.add("Golden", Default::default(), Vec::new());
        let id = app.presets.presets()[0].id;
        app.apply_ui_actions(vec![ui::UiAction::RequestDeletePreset(id)]);
        press(&mut app, ModifiersState::empty(), KeyCode::Digit3);
        assert_eq!(app.selected_rating(), 0);
        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert!(!app.confirm_open() && app.presets.get(id).is_some());

        app.apply_ui_actions(vec![ui::UiAction::RequestDeletePreset(id)]);
        press(&mut app, ModifiersState::empty(), KeyCode::NumpadEnter);
        assert!(!app.confirm_open() && app.presets.get(id).is_none());
    }

    #[test]
    fn cmd_comma_opens_settings_and_holds_other_keys_until_esc() {
        let (mut app, _) = folder_app(2);
        press(&mut app, CMD, KeyCode::Comma);
        assert!(app.show_settings());
        press(&mut app, ModifiersState::empty(), KeyCode::Space);
        assert_eq!(
            app.mode,
            ViewMode::Grid,
            "Space waits while Settings is open"
        );
        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        assert!(!app.show_settings());
        press(&mut app, CMD, KeyCode::Comma);
        press(&mut app, CMD, KeyCode::Comma);
        assert!(!app.show_settings(), "Cmd+, closes it again");
    }

    /// Real clicks through the whole UI: the landing page's Settings button
    /// opens the dialog, and its choices ask for a theme and a language. The
    /// theme and language actions are checked, not applied, because applying
    /// them would write the user's saved preferences.
    #[test]
    fn settings_opens_from_the_landing_page_and_its_choices_ask_for_changes() {
        use crate::app::test_support::{click, settled};
        use crate::i18n::{t, Lang};
        use crate::ui::{theme::Theme, UiAction};

        let mut app = App::new(None);
        let painted = settled(&mut app);
        let (actions, _) = click(&mut app, painted.pos_of(t().settings));
        assert_eq!(actions, vec![UiAction::ToggleSettings]);
        app.apply_ui_actions(actions);
        assert!(app.show_settings());

        let painted = settled(&mut app);
        assert!(
            painted.has(t().settings_theme) && painted.has(t().settings_language),
            "{:?}",
            painted.texts()
        );
        let (actions, painted) = click(&mut app, painted.pos_of(t().theme_light));
        assert_eq!(actions, vec![UiAction::SetTheme(Theme::Light)]);

        let (other, name) = match crate::i18n::lang() {
            Lang::En => (Lang::Zh, t().lang_chinese),
            Lang::Zh => (Lang::En, t().lang_english),
        };
        let (actions, painted) = click(&mut app, painted.pos_of(name));
        assert_eq!(actions, vec![UiAction::SetLanguage(other)]);

        assert!(
            !painted.has(t().settings_auto_tone),
            "Auto Tone's centering stays hidden: {:?}",
            painted.texts()
        );

        let (actions, _) = click(&mut app, painted.pos_of(t().close));
        assert_eq!(actions, vec![UiAction::CloseSettings]);
    }

    /// Settings follows the form layout: a section header over rows whose
    /// labels share one column and whose values start level with them.
    #[test]
    fn settings_lays_out_as_a_form() {
        use crate::app::test_support::settled;
        use crate::i18n::t;

        let mut app = App::new(None);
        app.apply_ui_actions(vec![crate::ui::UiAction::ToggleSettings]);
        let painted = settled(&mut app);
        let header = painted.pos_of(t().form_general);
        let theme = painted.pos_of(t().settings_theme);
        let language = painted.pos_of(t().settings_language);
        let dark = painted.pos_of(t().theme_dark);

        assert!(header.y < theme.y, "the section header sits above its rows");
        assert_eq!(theme.x, language.x, "labels share one column");
        assert!(dark.x > theme.x, "the value is right of its label");
        assert!(
            (dark.y - theme.y).abs() < 3.0,
            "the first value is level with its label"
        );
        assert!(
            language.y > painted.pos_of(t().theme_medium).y,
            "rows are top aligned, not centered"
        );
    }

    /// The Info page is a stacked form: each group's header above its rows,
    /// each label over its value, sharing its left edge.
    #[test]
    fn info_panel_lays_out_as_a_form() {
        use crate::app::test_support::settled;
        use crate::i18n::t;

        let (mut app, _) = folder_app(2);
        app.sel = Some(0);
        app.info_open = true;
        let painted = settled(&mut app);
        let header = painted.pos_of(t().info_file);
        let name = painted.pos_of(t().info_name);
        let value = painted.pos_of(
            &app.selected_path()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy(),
        );

        assert!(header.y < name.y, "the group title heads its rows");
        assert!(value.y > name.y, "the value is under its label");
        assert!(
            (value.x - name.x).abs() < 1.0,
            "the value shares its label's left edge"
        );
    }

    #[test]
    fn the_header_offers_settings_once_a_folder_is_open() {
        use crate::app::test_support::{click, settled};
        use crate::i18n::t;

        let (mut app, _) = folder_app(2);
        let painted = settled(&mut app);
        let (actions, _) = click(&mut app, painted.pos_of(t().settings));
        assert_eq!(actions, vec![crate::ui::UiAction::ToggleSettings]);
    }

    #[test]
    fn the_filmstrip_shows_one_dot_per_star() {
        use crate::app::test_support::settled;

        let (mut app, _) = editor_app();
        assert!(app.filmstrip_visible());
        let star = crate::ui::theme::colors(&app.egui_ctx).star;
        let dots = |app: &mut App| settled(app).circles_filled(star);

        assert!(dots(&mut app).is_empty(), "unrated photos show no dots");

        app.set_rating(3);
        let first = dots(&mut app);
        assert_eq!(first.len(), 3);

        app.sel = Some(1);
        app.set_rating(5);
        let both = dots(&mut app);
        assert_eq!(both.len(), 8);
        let first_right = first.iter().map(|p| p.x).fold(f32::MIN, f32::max);
        assert_eq!(
            both.iter().filter(|p| p.x > first_right).count(),
            5,
            "the second cell's dots sit right of the first's"
        );
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

    /// A bare-letter view key only acts unmodified, so Alt+G, Cmd+E and
    /// Alt+Y stay free rather than doubling as G, E and Y.
    #[test]
    fn view_letters_ignore_cmd_and_alt() {
        let (mut app, _) = folder_app(2);
        press(&mut app, CMD, KeyCode::KeyE);
        press(&mut app, ModifiersState::ALT, KeyCode::KeyE);
        assert_eq!(app.mode, ViewMode::Grid, "modified E does not open");

        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::ALT, KeyCode::KeyY);
        assert!(!app.compare, "Alt+Y does not compare");
        press(&mut app, ModifiersState::ALT, KeyCode::KeyG);
        assert_eq!(app.mode, ViewMode::Loupe, "Alt+G does not leave");
        press(&mut app, ModifiersState::empty(), KeyCode::KeyG);
        assert_eq!(app.mode, ViewMode::Grid);
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

    #[test]
    fn q_arms_touch_up_on_the_masks_tab_and_q_again_disarms() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyQ);
        assert!(app.touchup_active());
        assert_eq!(app.develop_tab(), DevelopTab::Masks);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyQ);
        assert!(!app.touchup_active());
        assert_eq!(app.develop_tab(), DevelopTab::Masks);
    }

    #[test]
    fn k_opens_masks_without_arming_touch_up() {
        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyK);
        assert_eq!(app.develop_tab(), DevelopTab::Masks);
        assert!(!app.touchup_active());
    }

    #[test]
    fn brackets_size_the_brush_while_touch_up_is_armed_and_rotate_after() {
        let (mut app, paths) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyQ);
        let start = app.touchup_radius();
        press(&mut app, ModifiersState::empty(), KeyCode::BracketRight);
        let grown = app.touchup_radius();
        assert!(grown > start, "{grown} > {start}");
        press(&mut app, ModifiersState::empty(), KeyCode::BracketLeft);
        assert!(app.touchup_radius() < grown);
        assert_eq!(
            app.rotations.get(&paths[0]),
            None,
            "no rotation while armed"
        );

        press(&mut app, ModifiersState::empty(), KeyCode::Escape);
        press(&mut app, ModifiersState::empty(), KeyCode::BracketRight);
        assert_eq!(app.rotations.get(&paths[0]), Some(&1));
    }

    #[test]
    fn shift_brackets_move_the_feather_within_its_range() {
        let (mut app, paths) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyQ);
        let radius = app.touchup_radius();
        assert_eq!(app.touchup_feather(), 1.0);
        press(&mut app, ModifiersState::SHIFT, KeyCode::BracketRight);
        assert_eq!(app.touchup_feather(), 1.0, "clamps at 1.0");
        press(&mut app, ModifiersState::SHIFT, KeyCode::BracketLeft);
        assert!((app.touchup_feather() - 0.9).abs() < 1e-5);
        for _ in 0..20 {
            press(&mut app, ModifiersState::SHIFT, KeyCode::BracketLeft);
        }
        assert_eq!(app.touchup_feather(), TOUCHUP_MIN_FEATHER);
        assert_eq!(app.touchup_radius(), radius, "feather keys leave the size");
        assert_eq!(app.rotations.get(&paths[0]), None);
    }

    #[test]
    fn o_shows_the_subject_overlay_and_shift_o_swaps_subject_and_background() {
        let (mut app, _) = editor_app();
        let supported = App::selection_supported();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyO);
        assert_eq!(app.selection_on(), supported);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyO);
        assert!(!app.selection_on());

        press(&mut app, ModifiersState::SHIFT, KeyCode::KeyO);
        assert_eq!(
            (app.selection_on(), app.selection_inverted()),
            (supported, supported),
            "Shift+O shows the overlay on the background"
        );
    }

    #[test]
    fn in_touch_up_o_hides_the_spots_and_shift_o_steps_through_them() {
        let (mut app, _) = editor_app();
        let spot = |u: f32| crate::develop::TouchUp {
            center: [u, 0.5],
            radius: 0.02,
            source: [u, 0.3],
            feather: 1.0,
            delta: [0.0; 3],
            opacity: 1.0,
        };
        app.apply_touchups(vec![spot(0.2), spot(0.5), spot(0.8)]);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyQ);

        press(&mut app, ModifiersState::empty(), KeyCode::KeyO);
        assert!(!app.touchup_spots_shown());
        assert!(
            !app.selection_on(),
            "O leaves the subject overlay alone in Touch Up"
        );
        press(&mut app, ModifiersState::empty(), KeyCode::KeyO);
        assert!(app.touchup_spots_shown());

        press(&mut app, ModifiersState::empty(), KeyCode::KeyO);
        let mut picked = Vec::new();
        for _ in 0..4 {
            press(&mut app, ModifiersState::SHIFT, KeyCode::KeyO);
            picked.push(app.touchup_selected());
        }
        assert_eq!(
            picked,
            [Some(0), Some(1), Some(2), Some(0)],
            "wraps after the last"
        );
        assert!(app.touchup_spots_shown(), "stepping shows the spots again");
    }

    #[test]
    fn digits_above_five_do_not_rate() {
        let (mut app, _) = folder_app(1);
        press(&mut app, ModifiersState::empty(), KeyCode::Digit3);
        for code in [
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
            KeyCode::Numpad9,
        ] {
            press(&mut app, ModifiersState::empty(), code);
            assert_eq!(app.selected_rating(), 3, "{code:?} keeps the rating");
        }
        app.set_rating(9);
        assert_eq!(app.selected_rating(), 3, "set_rating ignores 9");
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
        if !crate::app::SHOW_PRESETS {
            assert!(
                app.preset_name_edit().is_none(),
                "Cmd+Shift+P does nothing while presets are hidden"
            );
            return;
        }
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
    fn i_toggles_the_info_page_in_the_library_and_the_editor() {
        let (mut app, _) = folder_app(2);
        assert!(!app.info_open());
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.rail_lit(), Some(RailItem::Info));
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert!(!app.info_open());

        press(&mut app, CMD, KeyCode::KeyI);
        press(&mut app, ModifiersState::ALT, KeyCode::KeyI);
        assert!(!app.info_open(), "modified I is not the toggle");

        let (mut app, _) = editor_app();
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(app.rail_lit(), Some(RailItem::Info));
        assert_eq!(app.develop_page_shown(), None, "Info takes Develop's place");
        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert_eq!(
            app.develop_page_shown(),
            Some(DevelopTab::Sliders),
            "and closing it brings Develop back"
        );

        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        press(&mut app, ModifiersState::empty(), KeyCode::KeyX);
        assert_eq!(
            app.rail_lit(),
            Some(RailItem::Export),
            "Export replaces Info"
        );
        assert!(!app.info_open());
    }

    #[test]
    fn the_grid_rail_edits_the_selection_without_the_loupe_pages() {
        let (mut app, _) = folder_app(2);
        assert_eq!(app.rail_lit(), None, "the Grid opens with no page");
        app.click_rail(RailItem::Develop(DevelopTab::Masks));
        app.click_rail(RailItem::GroupCompare);
        assert_eq!(
            app.rail_lit(),
            None,
            "Clean up and Compare stay in the Loupe"
        );

        assert!(!app.grid_develop_needs_load(), "Info and Export don't load");
        app.click_rail(RailItem::Develop(DevelopTab::Sliders));
        assert_eq!(app.develop_page_shown(), Some(DevelopTab::Sliders));
        assert!(app.grid_develop_needs_load(), "Develop loads the selection");
        app.load_selected();
        assert!(!app.grid_develop_needs_load());
        press(&mut app, ModifiersState::empty(), KeyCode::ArrowRight);
        assert!(app.grid_develop_needs_load(), "and follows it");

        app.enter_loupe();
        app.set_develop_tab(DevelopTab::Masks);
        app.enter_grid();
        assert_eq!(
            app.develop_page_shown(),
            Some(DevelopTab::Sliders),
            "leaving the Loupe brings Develop back on Sliders"
        );
    }

    #[test]
    fn the_loupe_draws_no_folders_panel() {
        use crate::app::test_support::settled;
        use crate::i18n::t;

        let (mut app, _) = editor_app();
        let painted = settled(&mut app);
        assert!(!painted.has(t().metadata_title), "{:?}", painted.texts());
        for _ in 0..4 {
            press(&mut app, ModifiersState::empty(), KeyCode::F6);
            assert_ne!(app.focus, Region::Folders, "F6 skips the hidden tree");
        }

        press(&mut app, ModifiersState::empty(), KeyCode::KeyI);
        assert!(settled(&mut app).has(t().metadata_title));
    }

    #[test]
    fn the_grid_reads_metadata_only_while_the_info_page_shows_it() {
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
            "the Loupe's info bar reads either way"
        );
    }

    #[test]
    fn image_overflows_only_when_zoomed_past_the_window() {
        let (mut app, _) = editor_app();
        app.fit_to_window();
        assert!(!app.image_overflows(), "a fitted image has nothing to pan");
        app.zoom_by(3.0);
        assert!(app.image_overflows(), "3x fit is larger than the window");
    }

    #[test]
    fn the_wheel_does_not_zoom_while_touch_up_is_armed() {
        let (mut app, _) = editor_app();
        app.set_develop_tab(DevelopTab::Masks);
        app.tool = LoupeTool::TouchUp;
        app.on_scroll(0.0, 120.0);
        assert_zoom(&app, 0.5);

        app.tool = LoupeTool::None;
        app.on_scroll(0.0, 120.0);
        assert!(
            app.zoom() > 0.5,
            "the wheel zooms again once Touch Up is off"
        );
    }

    #[test]
    fn shift_wheel_does_not_pan_while_touch_up_is_armed() {
        let (mut app, _) = editor_app();
        app.set_develop_tab(DevelopTab::Masks);
        app.tool = LoupeTool::TouchUp;
        app.modifiers = ModifiersState::SHIFT;
        app.on_scroll(0.0, 120.0);
        assert_eq!(app.pan, (0.0, 0.0), "Shift+wheel feathers instead");

        app.tool = LoupeTool::None;
        app.on_scroll(0.0, 120.0);
        assert_ne!(app.pan, (0.0, 0.0), "Shift+wheel pans once Touch Up is off");
    }

    #[test]
    fn keyboard_slider_steps_bring_the_sliders_tab_forward() {
        let (mut app, _) = editor_app();
        app.set_develop_tab(DevelopTab::Masks);
        app.develop_move(1);
        assert_eq!(app.develop_tab(), DevelopTab::Sliders);
    }

    fn group_shapes(app: &App) -> Vec<(Vec<String>, String)> {
        let groups = app.catalog.groups().expect("groups loaded");
        groups
            .iter()
            .map(|(_, g)| {
                let s = |n: &std::ffi::OsString| n.to_string_lossy().into_owned();
                (g.members().iter().map(s).collect(), s(g.rep()))
            })
            .collect()
    }

    fn shape(members: &[&str], rep: &str) -> (Vec<String>, String) {
        (
            members.iter().map(|m| m.to_string()).collect(),
            rep.to_string(),
        )
    }

    #[test]
    fn cmd_g_with_one_cell_does_nothing() {
        let (mut app, _) = folder_app(4);
        press(&mut app, CMD, KeyCode::KeyG);
        assert!(group_shapes(&app).is_empty());
        assert_eq!(app.visible, vec![0, 1, 2, 3]);
        assert_eq!(app.mode, ViewMode::Grid);
    }

    #[test]
    fn cmd_g_in_the_loupe_stays_in_the_loupe() {
        let (mut app, _) = folder_app(3);
        app.enter_loupe();
        press(&mut app, CMD, KeyCode::KeyG);
        assert_eq!(app.mode, ViewMode::Loupe);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::KeyG);
        assert_eq!(app.mode, ViewMode::Loupe);
    }

    #[test]
    fn cmd_g_over_a_group_and_two_singles_changes_nothing() {
        let (mut app, _) = folder_app(6);
        group_photos(&mut app, &[1, 2, 3], 2);
        assert_eq!(app.visible, vec![0, 2, 4, 5]);
        app.selected = BTreeSet::from([0, 1, 2]);
        app.sel = Some(1);
        press(&mut app, CMD, KeyCode::KeyG);
        assert_eq!(
            group_shapes(&app),
            vec![shape(&["1.jpg", "2.jpg", "3.jpg"], "2.jpg")]
        );
        assert_eq!(app.visible, vec![0, 2, 4, 5]);
    }

    #[test]
    fn cmd_g_takes_the_primary_cells_photo_and_selects_the_new_cell() {
        let (mut app, _) = folder_app(5);
        app.selected = BTreeSet::from([1, 2, 3]);
        app.sel = Some(3);
        press(&mut app, CMD, KeyCode::KeyG);
        assert_eq!(
            group_shapes(&app),
            vec![shape(&["1.jpg", "2.jpg", "3.jpg"], "3.jpg")]
        );
        assert_eq!(app.visible, vec![0, 3, 4]);
        assert_eq!(app.selected, BTreeSet::from([1]));
        assert_eq!(app.sel, Some(1));
    }

    #[test]
    fn cmd_shift_g_restores_every_member_to_the_grid() {
        let (mut app, _) = folder_app(5);
        group_photos(&mut app, &[1, 2, 3], 2);
        app.selected = BTreeSet::from([0, 1]);
        app.sel = Some(0);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::KeyG);
        assert!(group_shapes(&app).is_empty());
        assert_eq!(app.visible, vec![0, 1, 2, 3, 4]);
        assert_eq!(app.selected, BTreeSet::from([0, 1, 2, 3]));
        assert_eq!(app.sel, Some(2), "the old representative is the cursor");
    }

    #[test]
    fn ungroup_then_group_restores_the_same_group() {
        let (mut app, _) = folder_app(5);
        group_photos(&mut app, &[1, 2, 3], 3);
        let before = group_shapes(&app);
        app.select_single(1);
        press(&mut app, CMD | ModifiersState::SHIFT, KeyCode::KeyG);
        press(&mut app, CMD, KeyCode::KeyG);
        assert_eq!(group_shapes(&app), before);
        assert_eq!(app.visible, vec![0, 3, 4]);
    }

    #[test]
    fn cmd_g_while_groups_load_changes_nothing() {
        let (mut app, paths) = folder_app(3);
        let dir = paths[0].parent().unwrap().to_path_buf();
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        app.mode = ViewMode::Grid;
        app.selected = BTreeSet::from([0, 1]);
        app.sel = Some(0);
        press(&mut app, CMD, KeyCode::KeyG);
        assert_eq!(
            app.status_text(),
            Some(crate::i18n::t().group_refused_loading)
        );
        assert_eq!(app.visible, vec![0, 1, 2]);
        crate::app::test_support::wait_for_catalog(&mut app);
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(group_shapes(&app).is_empty());
        assert!(!dir.join(".lightphotos").join("groups").exists());
    }
}
