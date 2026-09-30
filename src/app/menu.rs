//! The App side of the macOS menu bar: whether each command's key would act
//! right now, and running a command by replaying that key.

use super::*;

use crate::menu::MenuCommand;

impl App {
    /// Whether `cmd`'s key would act right now. Mirrors the early returns in
    /// `handle_key`.
    pub(crate) fn menu_enabled(&self, cmd: MenuCommand) -> bool {
        use MenuCommand::*;
        let typing = self.egui_ctx.text_edit_focused();
        // Crop, the WB picker, Touch Up, the preset-name prompt and a confirm
        // dialog each hold the keyboard. Settings holds it too, except for Cmd+,.
        let tool_free =
            self.tool == LoupeTool::None && self.preset_name_edit.is_none() && !self.confirm_open();
        let free = tool_free && self.crop_edit.is_none() && !self.show_settings;
        let loupe = self.mode == ViewMode::Loupe;
        let selected = self.selection_count();
        match cmd {
            Cut | Copy | Paste | BiggerText | SmallerText => true,
            SelectAll => typing || (free && self.mode == ViewMode::Grid),
            Undo => typing || (self.tool == LoupeTool::TouchUp && self.can_undo_touchup()),
            Settings => tool_free && self.crop_edit.is_none(),
            // X still opens the export form while cropping.
            Export => tool_free && !self.show_settings,
            OpenFolder | AutoTone | Rate(_) | Grid | KeyboardShortcuts => free,
            AutoToneSelection => free && selected > 0,
            MoveToTrash => free && selected > 0 && self.delete_available(),
            GroupSelected => free && self.selected_cells().len() >= 2,
            Ungroup => free && self.selection_has_group(),
            DeleteGroup => free && self.selection_has_group() && self.delete_available(),
            CopySettings => free && selected == 1,
            PasteSettings => free && selected > 0 && self.has_copied_settings(),
            Loupe => free && self.mode == ViewMode::Grid,
            InfoPanel => free && matches!(self.mode, ViewMode::Grid | ViewMode::Loupe),
            // [ and ] still rotate while cropping.
            RotateLeft | RotateRight => tool_free && !self.show_settings && loupe,
            BeforeAfter | ZoomToFit | ActualSize | ZoomIn | ZoomOut => free && loupe,
        }
    }

    /// Runs `cmd` as the keyboard would: the edit commands go to egui, as
    /// egui_winit turns their keys into events, and the rest go to
    /// `handle_key` with the command's modifiers held.
    pub(crate) fn run_menu_command(&mut self, cmd: MenuCommand) {
        if let Some(event) = self.text_edit_event(cmd) {
            if let Some(state) = self.egui_state.as_mut() {
                state.egui_input_mut().events.push(event);
            }
            self.request_redraw();
            return;
        }
        let (mods, code) = cmd.chord();
        let held = std::mem::replace(&mut self.modifiers, mods);
        self.handle_key(code);
        self.modifiers = held;
        self.request_redraw();
    }

    fn text_edit_event(&mut self, cmd: MenuCommand) -> Option<egui::Event> {
        let key = match cmd {
            MenuCommand::Cut => return Some(egui::Event::Cut),
            MenuCommand::Copy => return Some(egui::Event::Copy),
            MenuCommand::Paste => {
                let text = self.egui_state.as_mut()?.clipboard_text()?;
                return Some(egui::Event::Paste(text.replace("\r\n", "\n")));
            }
            MenuCommand::SelectAll => egui::Key::A,
            MenuCommand::Undo => egui::Key::Z,
            _ => return None,
        };
        self.egui_ctx
            .text_edit_focused()
            .then_some(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND,
            })
    }

    fn can_undo_touchup(&self) -> bool {
        self.shown
            .path()
            .and_then(|path| self.touchup_undo.get(path))
            .is_some_and(|steps| !steps.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::presets::tests::folder_app;

    #[test]
    fn a_rate_command_rates_the_selection_like_its_digit_key() {
        let (mut app, _, _) = folder_app("menu-rate", 2);
        app.select_single(1);
        app.modifiers = ModifiersState::SHIFT;
        app.run_menu_command(MenuCommand::Rate(3));
        assert_eq!(app.rating_at(1), 3);
        assert_eq!(app.rating_at(0), 0);
        assert_eq!(app.modifiers, ModifiersState::SHIFT, "modifiers restored");
    }

    #[test]
    fn open_settings_hold_every_command_but_settings() {
        let (mut app, _, _) = folder_app("menu-settings", 1);
        app.select_single(0);
        app.run_menu_command(MenuCommand::Settings);
        assert!(app.show_settings);
        assert!(!app.menu_enabled(MenuCommand::Rate(4)));
        app.run_menu_command(MenuCommand::Rate(4));
        assert_eq!(app.rating_at(0), 0, "settings kept the key from the photo");
        assert!(app.menu_enabled(MenuCommand::Settings));
        app.run_menu_command(MenuCommand::Settings);
        assert!(!app.show_settings);
        assert!(app.menu_enabled(MenuCommand::Rate(4)));
    }

    #[test]
    fn paste_settings_waits_for_a_copy() {
        let (mut app, _, _) = folder_app("menu-paste", 2);
        app.select_single(0);
        assert!(app.menu_enabled(MenuCommand::CopySettings));
        assert!(!app.menu_enabled(MenuCommand::PasteSettings));
        app.run_menu_command(MenuCommand::CopySettings);
        assert!(app.menu_enabled(MenuCommand::PasteSettings));
        app.selected = BTreeSet::from([0, 1]);
        assert!(
            !app.menu_enabled(MenuCommand::CopySettings),
            "copy needs one photo"
        );
    }
}
