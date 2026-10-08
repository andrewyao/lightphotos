//! The guided tour's state: which stop is shown. It runs only when asked,
//! from the header's Tour button or the home page's Take the Tour link.
//! `ui::tour` draws it.

use super::*;

use crate::ui::tour::TourStep;

impl App {
    /// The tour's shown stop, if it is running.
    pub(crate) fn tour_step(&self) -> Option<usize> {
        self.tour
    }

    /// Run the tour from its first stop. It points at the Grid's regions, so
    /// the Loupe goes back to the Grid, and a dialog over them closes.
    pub(crate) fn start_tour(&mut self) {
        if self.mode == ViewMode::Loupe {
            self.enter_grid();
        }
        self.show_settings = false;
        self.show_help = false;
        self.tour = Some(0);
        self.request_redraw();
    }

    pub(crate) fn tour_next(&mut self) {
        match self.tour {
            Some(i) if i + 1 < TourStep::ALL.len() => {
                self.tour = Some(i + 1);
                self.request_redraw();
            }
            Some(_) => self.end_tour(),
            None => {}
        }
    }

    pub(crate) fn tour_back(&mut self) {
        if let Some(i) = self.tour {
            self.tour = Some(i.saturating_sub(1));
            self.request_redraw();
        }
    }

    /// Skip or finish the tour.
    pub(crate) fn end_tour(&mut self) {
        self.tour = None;
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{click, settled};
    use crate::i18n::t;
    use crate::ui::UiAction;
    use winit::keyboard::{KeyCode, ModifiersState};

    #[test]
    fn next_and_back_walk_the_stops_and_next_on_the_last_ends_it() {
        let mut app = App::new(None);
        assert_eq!(app.tour_step(), None);
        app.start_tour();
        assert_eq!(app.tour_step(), Some(0));
        app.tour_back();
        assert_eq!(app.tour_step(), Some(0), "Back stops at the first");
        for i in 1..TourStep::ALL.len() {
            app.tour_next();
            assert_eq!(app.tour_step(), Some(i));
        }
        app.tour_back();
        assert_eq!(app.tour_step(), Some(TourStep::ALL.len() - 2));
        app.tour_next();
        app.tour_next();
        assert_eq!(app.tour_step(), None);
    }

    #[test]
    fn starting_the_tour_closes_settings_and_help() {
        let mut app = App::new(None);
        app.show_settings = true;
        app.show_help = true;
        app.start_tour();
        assert!(!app.show_settings() && !app.show_help());
    }

    /// Real clicks through the whole UI: the header's Tour button starts
    /// the tour, its Next button moves on, and Skip ends it.
    #[test]
    fn the_header_button_starts_the_tour_and_skip_ends_it() {
        let mut app = App::new(None);
        let painted = settled(&mut app);
        let (actions, _) = click(&mut app, painted.pos_of(t().tour));
        assert_eq!(actions, vec![UiAction::StartTour]);
        app.apply_ui_actions(actions);

        let painted = settled(&mut app);
        assert!(painted.has(t().tour_steps[0].0), "{:?}", painted.texts());
        let (actions, _) = click(&mut app, painted.pos_of(t().tour_next));
        assert_eq!(actions, vec![UiAction::TourNext]);
        app.apply_ui_actions(actions);
        assert_eq!(app.tour_step(), Some(1));

        let painted = settled(&mut app);
        assert!(painted.has(t().tour_steps[1].0), "{:?}", painted.texts());
        let (actions, _) = click(&mut app, painted.pos_of(t().skip_tour));
        assert_eq!(actions, vec![UiAction::EndTour]);
        app.apply_ui_actions(actions);
        assert_eq!(app.tour_step(), None);
    }

    #[test]
    fn the_home_page_button_under_open_folder_starts_the_tour() {
        let mut app = App::new(None);
        let painted = settled(&mut app);
        let (actions, _) = click(&mut app, painted.pos_of(t().take_tour));
        assert_eq!(actions, vec![UiAction::StartTour]);
    }

    /// The tour holds the keyboard: arrows move it, Esc ends it, and a
    /// shortcut such as Cmd+, waits.
    #[test]
    fn the_tour_owns_the_keyboard() {
        let mut app = App::new(None);
        app.start_tour();
        app.modifiers = ModifiersState::empty();
        app.handle_key(KeyCode::ArrowRight);
        assert_eq!(app.tour_step(), Some(1));
        app.handle_key(KeyCode::ArrowLeft);
        assert_eq!(app.tour_step(), Some(0));
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.tour_step(), Some(1));
        app.modifiers = ModifiersState::SUPER;
        app.handle_key(KeyCode::Comma);
        assert!(!app.show_settings(), "Settings waits for the tour");
        app.modifiers = ModifiersState::empty();
        app.handle_key(KeyCode::Escape);
        assert_eq!(app.tour_step(), None);
    }
}
