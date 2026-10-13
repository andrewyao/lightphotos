//! The app's state as one JSON object, for `--drive` and remote callers.

use super::{App, Region, ViewMode};

impl App {
    /// The `commands` listing, with the develop values of the photo on screen.
    pub(crate) fn listing(&self) -> crate::command::Listing {
        let current = self.shown.path().map(|_| self.current_adjustments());
        crate::command::listing(current.as_ref())
    }
}

/// What `--drive`'s `state` step prints: the app as a remote caller sees it.
#[derive(serde::Serialize)]
pub(crate) struct State {
    mode: ViewMode,
    /// Photos the filters leave in the Grid.
    visible: usize,
    /// Cell positions `[start, end)` the Grid drew last frame.
    grid_range: (usize, usize),
    /// Each cell in `grid_range` that carries a stack badge, as its
    /// position, its stack's size, and whether the stack is expanded.
    stacks: Vec<(usize, usize, bool)>,
    /// The primary photo's position among the visible ones.
    sel: Option<usize>,
    /// The multi-selection's positions.
    selected: Vec<usize>,
    /// File name of the photo single-photo actions apply to.
    photo: Option<String>,
    /// That photo's stars (0 when unrated), flag and color label.
    rating: Option<u8>,
    flag: Option<crate::sidecar::Flag>,
    label: Option<crate::sidecar::ColorLabel>,
    /// The keyboard-focused region.
    focus: Region,
    /// Loupe zoom relative to fit-to-window; absent in the Grid.
    zoom: Option<f32>,
    /// The status toast, while it shows.
    status: Option<String>,
    /// Visible thumbnails whose decode failed for good.
    failed_thumbs: usize,
    /// The open catalog's name.
    catalog: String,
    /// The Folders tree's roots.
    folder_roots: Vec<std::path::PathBuf>,
    /// The folder the Grid shows.
    folder: Option<std::path::PathBuf>,
}

impl State {
    pub(crate) fn of(app: &App) -> State {
        let mode = app.mode();
        let path = app.selected_path();
        State {
            mode,
            visible: app.visible_len(),
            grid_range: app.grid_range(),
            stacks: {
                let (start, end) = app.grid_range();
                (start..end)
                    .filter_map(|p| {
                        app.stack_badge_at(p)
                            .map(|(_, n, expanded)| (p, n, expanded))
                    })
                    .collect()
            },
            catalog: app.catalog.name().to_string(),
            folder_roots: app.folder_roots().to_vec(),
            folder: app.folder_sel(),
            sel: app.sel(),
            selected: app.selected_positions(),
            photo: path
                .as_ref()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            rating: path.as_ref().map(|p| app.rating_of(p)),
            flag: path.as_ref().and_then(|p| app.flag_of(p)),
            label: path.as_ref().and_then(|p| app.folder_store.label(p)),
            focus: app.focus(),
            zoom: (mode == ViewMode::Loupe).then(|| app.zoom_rel()),
            status: app.status().map(|(_, text)| text.to_string()),
            failed_thumbs: (0..app.visible_len())
                .filter(|&pos| app.thumb_failed_at(pos))
                .count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_of_a_fresh_app_is_the_empty_grid() {
        let app = App::new(None);
        let json = serde_json::to_string(&State::of(&app)).unwrap();
        assert_eq!(
            json,
            r#"{"mode":"grid","visible":0,"grid_range":[0,0],"stacks":[],"sel":null,"selected":[],"photo":null,"rating":null,"flag":null,"label":null,"focus":"folders","zoom":null,"status":null,"failed_thumbs":0,"catalog":"default","folder_roots":[],"folder":null}"#
        );
    }
}
