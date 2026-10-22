//! The App side of the macOS menu bar: whether each item would act right
//! now, and running it as a `Command`, by replaying its key, or directly for
//! the few items with no key.

use super::*;

use crate::command::Blocker;
use crate::shell::menu::MenuCommand;

impl App {
    /// Whether `cmd` would act right now. An item that runs a command asks
    /// `App::check`; the rest ask `input_blocker`, which holds the keyboard
    /// rules `handle_key` follows.
    pub(crate) fn menu_enabled(&self, cmd: MenuCommand) -> bool {
        use MenuCommand::*;
        let runs = || cmd.command().is_some_and(|c| self.check(&c).is_ok());
        let typing = self.egui_ctx.text_edit_focused();
        let blocker = self.input_blocker();
        let free = blocker.is_none();
        let loupe = self.mode == ViewMode::Loupe;
        let selected = self.selection_count();
        // A read-only folder offers no edit; the commands' own items are off
        // through `check`.
        let rw = !self.read_only();
        match cmd {
            Cut | Copy | Paste | BiggerText | SmallerText => true,
            SelectAll => typing || (free && self.mode == ViewMode::Grid),
            Undo => {
                typing
                    || (self.tool == LoupeTool::TouchUp && self.can_undo_touchup())
                    || (free && rw && self.history.can_undo())
            }
            Settings => matches!(blocker, None | Some(Blocker::Settings)),
            NewCatalog | OpenCatalog | OpenFolder | Export | KeyboardShortcuts => free,
            Crop => free && rw,
            CompareStack => {
                let on_stack = if loupe {
                    self.has_compare_set()
                } else {
                    self.sel.and_then(|pos| self.group_at(pos)).is_some()
                };
                let few = (2..=super::group_compare::COMPARE_SELECTION_MAX).contains(&selected);
                free && (few || on_stack)
            }
            AutoToneSelection => free && rw && selected > 0,
            ScorePhotos => free && rw && selected > 0 && self.scoring_available(),
            MoveToTrash => free && rw && selected > 0 && self.delete_available(),
            GroupSelected => free && rw && self.group_available(),
            GroupBursts => free && rw && self.group_bursts_available(),
            Ungroup => free && rw && self.selection_has_group(),
            DeleteGroup => free && rw && self.selection_has_group() && self.delete_available(),
            CopySettings => free && selected == 1,
            PasteSettings => free && rw && selected > 0 && self.has_copied_settings(),
            InfoPanel => free && matches!(self.mode, ViewMode::Grid | ViewMode::Loupe),
            BeforeAfter => free && loupe,
            Loupe => runs() && self.mode == ViewMode::Grid,
            DeselectAll => runs() && selected > 0,
            Rate(_) | Pick | Unflag | Reject | Label(_) | AutoTone | BlackAndWhite | RotateLeft
            | RotateRight | Grid | Develop | ToggleZoom | ZoomIn | ZoomOut => runs(),
        }
    }

    /// Runs `cmd` as the keyboard would: the edit commands go to egui, as
    /// egui_winit turns their keys into events, an item that stands for a
    /// `Command` runs it, and the rest go to `handle_key` with the item's
    /// modifiers held. An item with no key runs its action here.
    pub(crate) fn run_menu_command(&mut self, cmd: MenuCommand) {
        if let Some(event) = self.text_edit_event(cmd) {
            if let Some(state) = self.egui_state.as_mut() {
                state.egui_input_mut().events.push(event);
            }
            self.request_redraw();
            return;
        }
        if let Some(command) = cmd.command() {
            self.run_ui(command);
            self.request_redraw();
            return;
        }
        match cmd.chord() {
            Some((mods, code)) => {
                let held = std::mem::replace(&mut self.modifiers, mods);
                self.handle_key(code);
                self.modifiers = held;
            }
            None if !self.menu_enabled(cmd) => {}
            None => match cmd {
                MenuCommand::AutoToneSelection => self.request_bulk(ui::BulkKind::AutoTone),
                MenuCommand::ScorePhotos => self.score_selection(),
                MenuCommand::GroupBursts => self.group_bursts(),
                MenuCommand::NewCatalog => self.new_catalog_dialog(),
                MenuCommand::OpenCatalog => self.open_catalog_dialog(),
                _ => {}
            },
        }
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
    use crate::app::test_support::folder_app;

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
    fn a_command_with_no_key_runs_its_action() {
        let (mut app, _, _) = folder_app("menu-unbound", 2);
        app.selected = BTreeSet::from([0, 1]);
        assert!(MenuCommand::AutoToneSelection.chord().is_none());
        app.run_menu_command(MenuCommand::AutoToneSelection);
        assert!(app.confirm_open(), "Auto Adjust Selection asks first");
    }

    #[test]
    fn paste_settings_waits_for_a_copy() {
        let (mut app, _, _) = folder_app("menu-paste", 2);
        app.select_single(0);
        assert!(app.menu_enabled(MenuCommand::CopySettings));
        assert!(!app.menu_enabled(MenuCommand::PasteSettings));
        app.run_menu_command(MenuCommand::CopySettings);
        assert!(
            !app.menu_enabled(MenuCommand::PasteSettings),
            "not before Copy Settings is confirmed"
        );
        app.commit_copy_settings();
        assert!(app.menu_enabled(MenuCommand::PasteSettings));
        app.selected = BTreeSet::from([0, 1]);
        assert!(
            !app.menu_enabled(MenuCommand::CopySettings),
            "copy needs one photo"
        );
    }
}
