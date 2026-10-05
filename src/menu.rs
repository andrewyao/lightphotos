// SPDX-License-Identifier: GPL-3.0-or-later

//! The macOS menu bar. Each command stands for a key chord that
//! `App::handle_key` already binds, and choosing it replays that chord, so the
//! menu can do nothing the keyboard can't, under the same modal rules.
//!
//! AppKit offers a key event to the menu before the window. An enabled item
//! whose key equivalent matches consumes the key and winit never sees it; a
//! disabled one lets it through to winit. Either way a chord reaches
//! `handle_key` once.

use std::cell::{Cell, RefCell};

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;
use winit::keyboard::{KeyCode, ModifiersState};

use crate::app::App;
use crate::i18n::{self, Lang, MenuStrings};
use crate::macos_delegate::UserEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    Settings,
    OpenFolder,
    Export,
    Undo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    CopySettings,
    PasteSettings,
    Rate(u8),
    AutoTone,
    AutoToneSelection,
    ScorePhotos,
    RotateLeft,
    RotateRight,
    MoveToTrash,
    GroupSelected,
    Ungroup,
    DeleteGroup,
    Grid,
    Loupe,
    InfoPanel,
    BeforeAfter,
    ZoomToFit,
    ActualSize,
    ZoomIn,
    ZoomOut,
    BiggerText,
    SmallerText,
    KeyboardShortcuts,
}

impl MenuCommand {
    /// The chord `App::handle_key` binds to this command.
    pub fn chord(self) -> (ModifiersState, KeyCode) {
        use MenuCommand::*;
        let none = ModifiersState::empty();
        let cmd = ModifiersState::SUPER;
        let shift_cmd = ModifiersState::SUPER | ModifiersState::SHIFT;
        let alt = ModifiersState::ALT;
        match self {
            Settings => (cmd, KeyCode::Comma),
            OpenFolder => (cmd, KeyCode::KeyO),
            Export => (none, KeyCode::KeyX),
            Undo => (cmd, KeyCode::KeyZ),
            Cut => (cmd, KeyCode::KeyX),
            Copy => (cmd, KeyCode::KeyC),
            Paste => (cmd, KeyCode::KeyV),
            SelectAll => (cmd, KeyCode::KeyA),
            CopySettings => (shift_cmd, KeyCode::KeyC),
            PasteSettings => (shift_cmd, KeyCode::KeyY),
            Rate(stars) => (none, DIGITS[stars.min(5) as usize]),
            AutoTone => (cmd, KeyCode::KeyU),
            AutoToneSelection => (shift_cmd, KeyCode::KeyU),
            ScorePhotos => (shift_cmd, KeyCode::KeyS),
            RotateLeft => (none, KeyCode::BracketLeft),
            RotateRight => (none, KeyCode::BracketRight),
            MoveToTrash => (none, KeyCode::Delete),
            GroupSelected => (cmd, KeyCode::KeyG),
            Ungroup => (shift_cmd, KeyCode::KeyG),
            DeleteGroup => (ModifiersState::SHIFT, KeyCode::Delete),
            Grid => (none, KeyCode::KeyG),
            Loupe => (none, KeyCode::KeyE),
            InfoPanel => (none, KeyCode::KeyI),
            BeforeAfter => (none, KeyCode::KeyY),
            ZoomToFit => (cmd, KeyCode::Digit0),
            ActualSize => (cmd, KeyCode::Digit1),
            ZoomIn => (cmd, KeyCode::Equal),
            ZoomOut => (cmd, KeyCode::Minus),
            BiggerText => (alt, KeyCode::Equal),
            SmallerText => (alt, KeyCode::Minus),
            KeyboardShortcuts => (ModifiersState::SHIFT, KeyCode::Slash),
        }
    }

    /// Only a chord with Cmd or Alt becomes a key equivalent. The menu sees
    /// keys before a focused text field does, so a bare letter or `?` there
    /// would stop the user typing it.
    fn accelerator(self) -> Option<Accelerator> {
        let (mods, key) = self.chord();
        if !(mods.super_key() || mods.alt_key()) {
            return None;
        }
        let mut accel = Modifiers::empty();
        for (held, bit) in [
            (mods.super_key(), Modifiers::META),
            (mods.shift_key(), Modifiers::SHIFT),
            (mods.alt_key(), Modifiers::ALT),
        ] {
            if held {
                accel |= bit;
            }
        }
        let code = match key {
            KeyCode::KeyA => Code::KeyA,
            KeyCode::KeyC => Code::KeyC,
            KeyCode::KeyG => Code::KeyG,
            KeyCode::KeyO => Code::KeyO,
            KeyCode::KeyS => Code::KeyS,
            KeyCode::KeyU => Code::KeyU,
            KeyCode::KeyV => Code::KeyV,
            KeyCode::KeyX => Code::KeyX,
            KeyCode::KeyY => Code::KeyY,
            KeyCode::KeyZ => Code::KeyZ,
            KeyCode::Comma => Code::Comma,
            KeyCode::Digit0 => Code::Digit0,
            KeyCode::Digit1 => Code::Digit1,
            KeyCode::Equal => Code::Equal,
            KeyCode::Minus => Code::Minus,
            _ => return None,
        };
        Some(Accelerator::new(accel, code))
    }

    fn id(self) -> String {
        format!("{self:?}")
    }
}

const DIGITS: [KeyCode; 6] = [
    KeyCode::Digit0,
    KeyCode::Digit1,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
];

type Label = fn(&MenuStrings) -> &'static str;

/// An AppKit-provided item, which acts on the app or window itself.
#[derive(Clone, Copy)]
enum System {
    About,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Minimize,
    Zoom,
}

enum Row {
    Command(MenuCommand, Label),
    System(System, Label),
    Separator,
    Submenu(Label, &'static [Row]),
}

/// Which AppKit role a top-level menu takes, for the window list and the
/// Help search field.
#[derive(PartialEq)]
enum Role {
    Plain,
    Windows,
    Help,
}

use MenuCommand as C;
use Row::{Command, Separator};

const MENUS: &[(Label, Role, &[Row])] = &[
    (
        |_| "LightPhotos",
        Role::Plain,
        &[
            Row::System(System::About, |m| m.about),
            Command(C::Settings, |m| m.settings),
            Separator,
            Row::System(System::Hide, |m| m.hide),
            Row::System(System::HideOthers, |m| m.hide_others),
            Row::System(System::ShowAll, |m| m.show_all),
            Separator,
            Row::System(System::Quit, |m| m.quit),
        ],
    ),
    (
        |m| m.file,
        Role::Plain,
        &[
            Command(C::OpenFolder, |m| m.open_folder),
            Command(C::Export, |m| m.export),
        ],
    ),
    (
        |m| m.edit,
        Role::Plain,
        &[
            Command(C::Undo, |m| m.undo),
            Separator,
            Command(C::Cut, |m| m.cut),
            Command(C::Copy, |m| m.copy),
            Command(C::Paste, |m| m.paste),
            Command(C::SelectAll, |m| m.select_all),
            Separator,
            Command(C::CopySettings, |m| m.copy_settings),
            Command(C::PasteSettings, |m| m.paste_settings),
        ],
    ),
    (
        |m| m.photo,
        Role::Plain,
        &[
            Row::Submenu(
                |m| m.rate,
                &[
                    Command(C::Rate(0), |m| m.no_rating),
                    Command(C::Rate(1), |_| "\u{2605}"),
                    Command(C::Rate(2), |_| "\u{2605}\u{2605}"),
                    Command(C::Rate(3), |_| "\u{2605}\u{2605}\u{2605}"),
                    Command(C::Rate(4), |_| "\u{2605}\u{2605}\u{2605}\u{2605}"),
                    Command(C::Rate(5), |_| "\u{2605}\u{2605}\u{2605}\u{2605}\u{2605}"),
                ],
            ),
            Command(C::AutoTone, |m| m.auto_tone),
            Command(C::AutoToneSelection, |m| m.auto_tone_selection),
            Command(C::ScorePhotos, |m| m.score_photos),
            Command(C::RotateLeft, |m| m.rotate_left),
            Command(C::RotateRight, |m| m.rotate_right),
            Command(C::MoveToTrash, |m| m.move_to_trash),
            Separator,
            Command(C::GroupSelected, |m| m.group_selected),
            Command(C::Ungroup, |m| m.ungroup),
            Command(C::DeleteGroup, |m| m.delete_group),
        ],
    ),
    (
        |m| m.view,
        Role::Plain,
        &[
            Command(C::Grid, |m| m.grid),
            Command(C::Loupe, |m| m.loupe),
            Command(C::InfoPanel, |m| m.info_panel),
            Command(C::BeforeAfter, |m| m.before_after),
            Separator,
            Command(C::ZoomToFit, |m| m.zoom_to_fit),
            Command(C::ActualSize, |m| m.actual_size),
            Command(C::ZoomIn, |m| m.zoom_in),
            Command(C::ZoomOut, |m| m.zoom_out),
            Separator,
            Command(C::BiggerText, |m| m.bigger_text),
            Command(C::SmallerText, |m| m.smaller_text),
        ],
    ),
    (
        |m| m.window,
        Role::Windows,
        &[
            Row::System(System::Minimize, |m| m.minimize),
            Row::System(System::Zoom, |m| m.zoom),
        ],
    ),
    (
        |m| m.help,
        Role::Help,
        &[Command(C::KeyboardShortcuts, |m| m.keyboard_shortcuts)],
    ),
];

/// Every command in the table, in menu order.
fn commands() -> Vec<MenuCommand> {
    fn walk(rows: &[Row], out: &mut Vec<MenuCommand>) {
        for row in rows {
            match row {
                Command(cmd, _) => out.push(*cmd),
                Row::Submenu(_, rows) => walk(rows, out),
                Row::System(..) | Separator => {}
            }
        }
    }
    let mut out = Vec::new();
    for (_, _, rows) in MENUS {
        walk(rows, &mut out);
    }
    out
}

struct MenuBar {
    lang: Lang,
    _menu: Menu,
    /// Each command's item and the enabled state last pushed to AppKit.
    items: Vec<(MenuCommand, MenuItem, Cell<bool>)>,
}

thread_local! {
    /// AppKit menus belong to the main thread, which is the only one that
    /// touches this.
    static BAR: RefCell<Option<MenuBar>> = const { RefCell::new(None) };
}

/// Build the menu bar and route its commands into the event loop. Call on the
/// main thread once the event loop exists, with winit's default menu turned
/// off, since winit would otherwise replace this one when the app finishes
/// launching.
pub fn install(proxy: EventLoopProxy<UserEvent>) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(cmd) = commands().into_iter().find(|c| c.id() == event.id().0) {
            let _ = proxy.send_event(UserEvent::Menu(cmd));
        }
    }));
    BAR.with_borrow_mut(|bar| *bar = Some(build(i18n::lang())));
}

/// Match each item's enabled state to `App::menu_enabled`, and rebuild the
/// menus after a language change.
pub fn refresh(app: &App) {
    BAR.with_borrow_mut(|bar| {
        let Some(current) = bar else { return };
        if current.lang != i18n::lang() {
            *current = build(i18n::lang());
        }
        for (cmd, item, shown) in &current.items {
            let enabled = app.menu_enabled(*cmd);
            if enabled != shown.get() {
                item.set_enabled(enabled);
                shown.set(enabled);
            }
        }
    });
}

fn build(lang: Lang) -> MenuBar {
    let m = &i18n::t().menu;
    let menu = Menu::new();
    let mut items = Vec::new();
    for (title, role, rows) in MENUS {
        let submenu = Submenu::new(title(m), true);
        append(&submenu, rows, m, &mut items);
        menu.append(&submenu).expect("append menu");
        match role {
            Role::Windows => submenu.set_as_windows_menu_for_nsapp(),
            Role::Help => submenu.set_as_help_menu_for_nsapp(),
            Role::Plain => {}
        }
    }
    menu.init_for_nsapp();
    MenuBar {
        lang,
        _menu: menu,
        items,
    }
}

fn append(
    parent: &Submenu,
    rows: &[Row],
    m: &MenuStrings,
    items: &mut Vec<(MenuCommand, MenuItem, Cell<bool>)>,
) {
    for row in rows {
        let appended = match row {
            Command(cmd, label) => {
                let item = MenuItem::with_id(cmd.id(), label(m), true, cmd.accelerator());
                let appended = parent.append(&item);
                items.push((*cmd, item, Cell::new(true)));
                appended
            }
            Row::System(system, label) => parent.append(&system_item(*system, label(m))),
            Separator => parent.append(&PredefinedMenuItem::separator()),
            Row::Submenu(label, rows) => {
                let submenu = Submenu::new(label(m), true);
                append(&submenu, rows, m, items);
                parent.append(&submenu)
            }
        };
        appended.expect("append menu item");
    }
}

fn system_item(system: System, text: &str) -> PredefinedMenuItem {
    let text = Some(text);
    match system {
        System::About => PredefinedMenuItem::about(
            text,
            Some(AboutMetadata {
                name: Some("LightPhotos".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Default::default()
            }),
        ),
        System::Hide => PredefinedMenuItem::hide(text),
        System::HideOthers => PredefinedMenuItem::hide_others(text),
        System::ShowAll => PredefinedMenuItem::show_all(text),
        System::Quit => PredefinedMenuItem::quit(text),
        System::Minimize => PredefinedMenuItem::minimize(text),
        System::Zoom => PredefinedMenuItem::maximize(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cmd_and_alt_chords_become_key_equivalents() {
        for cmd in commands() {
            let (mods, _) = cmd.chord();
            let chorded = mods.super_key() || mods.alt_key();
            assert_eq!(
                cmd.accelerator().is_some(),
                chorded,
                "{cmd:?} key equivalent"
            );
        }
    }

    #[test]
    fn menu_ids_name_one_command_each() {
        let all = commands();
        for cmd in &all {
            assert_eq!(
                all.iter().filter(|c| c.id() == cmd.id()).count(),
                1,
                "{cmd:?}"
            );
        }
    }
}
