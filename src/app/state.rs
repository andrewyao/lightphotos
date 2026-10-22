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
    /// That photo's stored quality score, where scoring runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<Score>,
    /// `[done, total]` while a scoring run is going.
    #[serde(skip_serializing_if = "Option::is_none")]
    scoring: Option<(usize, usize)>,
    /// That photo's masks, and the one the Masks page edits.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    masks: Vec<crate::develop::Mask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_mask: Option<usize>,
    /// The keyboard-focused region.
    focus: Region,
    /// Whether the Loupe shows only the photo; absent when it doesn't.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    fullscreen: bool,
    /// Whether a slide show plays; absent when it doesn't.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    slideshow: bool,
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

/// What the control channel's `list` op answers: each photo the Grid shows,
/// in its order. A closed stack shows as its cover.
#[derive(serde::Serialize)]
pub(crate) struct PhotoList {
    photos: Vec<Photo>,
    /// Photos the Grid shows, listed or not.
    total: usize,
    /// Whether `photos` stops at [`PhotoList::MAX`].
    truncated: bool,
}

#[derive(serde::Serialize)]
struct Photo {
    name: String,
    /// 0 when unrated.
    rating: u8,
    flag: Option<crate::sidecar::Flag>,
    label: Option<crate::sidecar::ColorLabel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<Score>,
    /// Whether it has develop edits.
    edited: bool,
}

impl PhotoList {
    /// Keeps one reply to a few hundred KB.
    const MAX: usize = 5000;

    #[cfg(test)]
    pub(crate) fn of(app: &App) -> PhotoList {
        PhotoList::showing(app, |_| false)
    }

    /// What an agent sees (`agent.rs`): nothing when the open folder is
    /// outside its anchor, and only the name of a photo that links out.
    pub(crate) fn for_agent(
        app: &App,
        folder_held: bool,
        hidden: impl Fn(&std::path::Path) -> bool,
    ) -> PhotoList {
        match folder_held {
            true => PhotoList::showing(app, hidden),
            false => PhotoList {
                photos: Vec::new(),
                total: 0,
                truncated: false,
            },
        }
    }

    /// The list, with only the names of the photos `hidden` picks.
    fn showing(app: &App, hidden: impl Fn(&std::path::Path) -> bool) -> PhotoList {
        let total = app.visible.len();
        let photos = match app.playlist.as_ref() {
            None => Vec::new(),
            Some(pl) => app
                .visible
                .iter()
                .take(PhotoList::MAX)
                .enumerate()
                .filter_map(|(pos, &idx)| {
                    let path = pl.entry(idx)?;
                    if hidden(path) {
                        return Some(Photo {
                            name: path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            rating: 0,
                            flag: None,
                            label: None,
                            score: None,
                            edited: false,
                        });
                    }
                    Some(Photo {
                        name: path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        rating: app.rating_of(path),
                        flag: app.flag_of(path),
                        label: app.folder_store.label(path),
                        score: app.score_at(pos).map(|(s, stale)| Score {
                            value: s.value,
                            stale,
                        }),
                        edited: app.folder_store.edited(path).is_some(),
                    })
                })
                .collect(),
        };
        PhotoList {
            photos,
            total,
            truncated: total > PhotoList::MAX,
        }
    }
}

#[derive(serde::Serialize)]
struct Score {
    value: u8,
    /// Whether the photo was edited after it was scored.
    stale: bool,
}

impl State {
    /// What an agent sees (`agent.rs`): the catalog folders in its reach,
    /// no folder or photo when the open folder is outside its anchor, and
    /// none of the shown photo's marks, score or masks when it links out.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn for_agent(
        mut self,
        roots: Vec<std::path::PathBuf>,
        folder_held: bool,
        photo_held: bool,
    ) -> State {
        self.folder_roots = roots;
        if !folder_held {
            self.folder = None;
            self.photo = None;
        }
        if !(folder_held && photo_held) {
            self.rating = None;
            self.flag = None;
            self.label = None;
            self.score = None;
            self.masks.clear();
            self.selected_mask = None;
        }
        self
    }

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
            score: app.shown_score().map(|(s, stale)| Score {
                value: s.value,
                stale,
            }),
            scoring: app.score_progress(),
            masks: path
                .as_ref()
                .and_then(|p| app.masks.get(p).cloned())
                .unwrap_or_default(),
            selected_mask: app.selected_mask(),
            focus: app.focus(),
            fullscreen: app.fullscreen_shown(),
            slideshow: app.slideshow_playing(),
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
    use crate::app::test_support::folder_app;
    use serde_json::json;

    #[test]
    fn the_list_is_every_photo_the_grid_shows_in_its_order() {
        let (mut app, dir, paths) = folder_app("state-list", 3);
        app.set_rating_of(paths[0].clone(), 2);
        app.set_rating_of(paths[1].clone(), 4);
        app.folder_store
            .set_flag(&paths[1], Some(crate::sidecar::Flag::Pick));
        app.folder_store
            .set_label(&paths[2], Some(crate::sidecar::ColorLabel::Red));
        app.folder_store.set_edited(&paths[2], 1);
        app.set_sort(crate::app::GridSort::Rating);

        let list = serde_json::to_value(PhotoList::of(&app)).unwrap();
        assert_eq!(
            list,
            json!({"photos": [
                {"name": "1.jpg", "rating": 4, "flag": "pick", "label": null, "edited": false},
                {"name": "0.jpg", "rating": 2, "flag": null, "label": null, "edited": false},
                {"name": "2.jpg", "rating": 0, "flag": null, "label": "red", "edited": true},
            ], "total": 3, "truncated": false})
        );
        app.set_filter(Some((crate::navigation::Cmp::Gte, 3)));
        let list = serde_json::to_value(PhotoList::of(&app)).unwrap();
        assert_eq!(list["photos"].as_array().map(Vec::len), Some(1));
        assert_eq!(list["photos"][0]["name"], "1.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_past_its_cap_says_it_is_cut_short() {
        let (app, dir, _) = folder_app("state-list-cap", PhotoList::MAX + 1);
        let list = PhotoList::of(&app);
        assert_eq!(
            (list.photos.len(), list.total, list.truncated),
            (PhotoList::MAX, PhotoList::MAX + 1, true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

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
