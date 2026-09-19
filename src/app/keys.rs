use super::*;

use winit::event_loop::ActiveEventLoop;
use winit::keyboard::KeyCode;

use crate::navigation::Cmp;
use crate::ui;

impl App {
    /// Whether an arrow or Tab key that egui marked "consumed" should still reach
    /// app navigation. egui keeps focus on a slider after a drag and consumes
    /// every later arrow key, and egui_winit consumes every Tab press. We ignore
    /// that unless a slider is being dragged right now or a crop is in progress.
    pub(crate) fn nav_key_should_fall_through(&self) -> bool {
        self.crop_edit.is_none() && !self.egui_ctx.egui_is_using_pointer()
    }

    /// Handle a key press per the Lightroom key-binding table.
    pub(crate) fn handle_key(&mut self, code: KeyCode, _event_loop: &ActiveEventLoop) {
        let shift = self.modifiers.shift_key();
        #[cfg(target_arch = "wasm32")]
        let cmd = self.modifiers.super_key() || self.modifiers.control_key();
        #[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
        let cmd = self.modifiers.super_key();
        #[cfg(all(not(target_arch = "wasm32"), not(target_os = "macos")))]
        let cmd = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();

        // While cropping, other keys do nothing, so a stray arrow or digit
        // can't move the selection out from under the crop.
        if self.crop_edit.is_some() {
            match code {
                KeyCode::KeyC | KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.cancel_crop(),
                KeyCode::KeyX if !cmd && !alt => self.export_selected(),
                _ => {}
            }
            return;
        }

        // While the WB picker is armed, only Escape (cancel) does anything.
        if self.wb_picker {
            if code == KeyCode::Escape {
                self.wb_picker = false;
                self.request_redraw();
            }
            return;
        }

        if self.touchup_active {
            match code {
                KeyCode::Escape => {
                    self.touchup_active = false;
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

        // Shift+1..5 filters to rating >= N and Shift+0 clears the filter.
        // `set_filter` ignores this in Loupe.
        if shift {
            if let Some(n) = digit_of(code) {
                if (1..=5).contains(&n) {
                    self.set_filter(Some((Cmp::Gte, n)));
                    return;
                }
                if n == 0 {
                    self.set_filter(None);
                    return;
                }
            }
        }

        // Plain digits set the rating, and 0 clears it.
        if !shift && !cmd && !alt {
            if let Some(n) = digit_of(code) {
                self.set_rating(n);
                return;
            }
        }

        match code {
            KeyCode::Digit0 if alt => self.reset_100(),
            KeyCode::BracketLeft if cmd && self.mode == ViewMode::Loupe => self.rotate(false),
            KeyCode::BracketRight if cmd && self.mode == ViewMode::Loupe => self.rotate(true),

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
            KeyCode::KeyB if !cmd && !alt => self.toggle_bursts(),
            KeyCode::KeyD if !cmd && !alt => self.toggle_dupes(),
            KeyCode::KeyE => {
                if self.mode == ViewMode::Grid {
                    self.enter_loupe();
                }
            }
            KeyCode::KeyC if !cmd && !alt => self.enter_crop(),
            KeyCode::KeyX if !cmd && !alt => self.export_selected(),
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
                } else if self.focus == Region::Grid && self.focus_level == FocusLevel::Selected {
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
