use super::*;

use winit::event_loop::ActiveEventLoop;
use winit::keyboard::KeyCode;

use crate::navigation::Cmp;
use crate::ui;

impl App {
    /// Whether an arrow key or Tab that egui reported as "consumed" should still
    /// reach the app's own navigation. egui keeps keyboard focus on a develop
    /// `Slider` after the user drags it, and then flags every subsequent arrow
    /// key as consumed — which silently kills image navigation until the slider
    /// loses focus; egui_winit separately hardcodes every Tab press as consumed
    /// regardless of focus (see the Tab-stripping comment in `redraw`). We let
    /// both fall through unless a slider is being *actively* dragged right now
    /// (`is_using_pointer`) or a crop is in progress.
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

        // While cropping, the keyboard is limited to the crop sub-mode: `C`/Enter
        // commit, `Esc` cancels, `X` still exports. Everything else is inert so a
        // stray arrow/digit can't move the selection out from under the crop.
        if self.crop_edit.is_some() {
            match code {
                KeyCode::KeyC | KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.cancel_crop(),
                KeyCode::KeyX if !cmd && !alt => self.export_selected(),
                _ => {}
            }
            return;
        }

        // While the WB picker is armed, only Escape does anything (cancels
        // it); everything else is inert until a pixel is clicked.
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

        // `?` (Shift+/) toggles the shortcut-help overlay; Esc closes it if open.
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

        // While the quit-confirmation modal is up the app is otherwise inert.
        // Escape confirms Quit — repeated Escape from anywhere naturally backs
        // all the way out of the app, mirroring the Quit button's action —
        // and Enter cancels, keeping "proceed into the app" consistent with
        // every other Enter binding.
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

        // Survey Mode is a modal-like screen entered from/exited to the Grid:
        // Left/Right move which member the shared rating hotkeys apply to
        // (via `selected_path`'s Survey branch), Escape closes it, Enter runs
        // the one-click keep-best action. Digit ratings fall through to the
        // shared block below unchanged.
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

        // Shift+1..5 → filter ≥ N; Shift+0 → clear. Grid/Survey only —
        // `set_filter` itself no-ops in Loupe (see its doc comment), so this
        // is dispatched unconditionally rather than mode-checked here too.
        // Checked before plain digits.
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

        // Plain digits rate (0 clears) in both modes — never zoom.
        if !shift && !cmd && !alt {
            if let Some(n) = digit_of(code) {
                self.set_rating(n);
                return;
            }
        }

        match code {
            // Loupe-only existing transforms.
            KeyCode::Digit0 if alt => self.reset_100(),
            KeyCode::BracketLeft if cmd && self.mode == ViewMode::Loupe => self.rotate(false),
            KeyCode::BracketRight if cmd && self.mode == ViewMode::Loupe => self.rotate(true),

            // F6 toggles keyboard focus between whatever main region you're in
            // (Folders/Grid/Detail/Develop) and the two chrome regions
            // (Toolbar, then Filmstrip) when a region is merely "selected";
            // once a region is "entered", F6 instead moves within it
            // (Toolbar's control cursor — other entered regions have nothing
            // for F6 to do there, since arrows/Tab already cover their
            // content). See `cycle_region`/`cycle_control`.
            KeyCode::F6 => match self.focus_level {
                FocusLevel::Selected => self.cycle_region(shift),
                FocusLevel::Entered => self.cycle_control(shift),
            },
            // Tab/Shift+Tab cycle between selectable items *within* whichever
            // region has focus (never between regions — that's F6's job).
            // egui_winit hardcodes every Tab press as `consumed` to run its
            // own competing Tab-driven widget-focus traversal (see the
            // Tab-stripping comment in `redraw`), so these ride the same
            // `nav_key_should_fall_through` path in `main.rs` that arrows use.
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

            // Cmd/Ctrl+O: open the folder picker (same as the toolbar "Open"
            // button and the landing page). Works in any mode.
            KeyCode::KeyO if cmd && !alt => self.open_folder_picker(),

            KeyCode::KeyG => self.enter_grid(),
            // `B` toggles best-of-burst badges (grid). No-op while a filter is
            // active — `toggle_bursts` guards it.
            KeyCode::KeyB if !cmd && !alt => self.toggle_bursts(),
            // `D` toggles content-duplicate (dHash) grouping badges (grid).
            KeyCode::KeyD if !cmd && !alt => self.toggle_dupes(),
            // `E` is the focus-independent "enter loupe" edit key (Lightroom).
            KeyCode::KeyE => {
                if self.mode == ViewMode::Grid {
                    self.enter_loupe();
                }
            }
            // `C` enters crop mode (opening the loupe first from the grid).
            KeyCode::KeyC if !cmd && !alt => self.enter_crop(),
            // `X` exports the selected image as a baked JPG, in either mode.
            KeyCode::KeyX if !cmd && !alt => self.export_selected(),
            // Enter is focus-dependent (open image / expand folder / …).
            KeyCode::Enter | KeyCode::NumpadEnter => self.nav_enter(),
            // Cmd+Shift+Y applies the copied develop settings to the whole
            // selection (only when something has been copied; checked before
            // plain `Y` so the compare arm can't eat the Cmd+Shift chord).
            KeyCode::KeyY if cmd && shift && self.has_copied_settings() => {
                self.request_bulk(ui::BulkKind::ApplySettings)
            }
            // `Y` toggles the before/after compare view (Loupe only).
            KeyCode::KeyY if self.mode == ViewMode::Loupe && !cmd => self.toggle_compare(),
            // Escape is the exact inverse of Enter: exactly one step back per
            // press, all the way out to a quit prompt (native only — see the
            // last arm). Priority order:
            // chrome (Toolbar/Filmstrip) returns to the remembered main
            // region (mirrors F6's toggle-back); Develop moves focus back to
            // Detail (the panel stays visible — only keyboard focus moves);
            // Detail drops back to Grid; Grid at `Selected` (nothing left for
            // the generic rule to pop) goes to Folders and withdraws the
            // selection, since leaving the grid means nothing is "the
            // selected photo" anymore; otherwise the generic focus-level rule
            // pops Entered back to Selected; and finally, already just
            // Selected on Folders with nothing left to pop, ask to quit (on
            // wasm there is nothing to quit, so that last step does nothing).
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
                    // A browser tab has no "quit" for us to offer, so the last
                    // Escape there is simply a no-op.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        self.pending_quit = true;
                    }
                }
                self.request_redraw();
            }

            // Cmd+A selects every visible cell in the grid.
            KeyCode::KeyA if cmd && self.mode == ViewMode::Grid => self.select_all(),
            // Cmd+Shift+C copies the primary photo's develop settings.
            KeyCode::KeyC if cmd && shift => self.copy_settings(),
            // Delete moves the selection to the Trash (after confirm).
            KeyCode::Delete => self.request_bulk(ui::BulkKind::Delete),

            KeyCode::ArrowLeft => self.nav_arrow(-1, 0, shift),
            KeyCode::ArrowRight => self.nav_arrow(1, 0, shift),
            KeyCode::ArrowUp => self.nav_arrow(0, -1, shift),
            KeyCode::ArrowDown => self.nav_arrow(0, 1, shift),

            // Page Up/Down step to the prev/next image in Detail or Develop
            // (Grid keeps Arrow/Tab-only stepping).
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
