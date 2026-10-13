// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Grid's order: what it sorts by and which way, remembered across
//! launches.

use super::{App, ViewMode};

/// What the Grid sorts by. Photos with no value for it go last in name
/// order, whichever way the sort runs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum GridSort {
    /// The folder's file-name order.
    #[default]
    Name,
    Time,
    Modified,
    Size,
    Shutter,
    /// The quality score; unscored photos have none.
    Quality,
    /// Stars, unrated counting as zero.
    Rating,
    /// When the photo's edits last changed; unedited photos have none.
    Edited,
}

impl GridSort {
    const ALL: [GridSort; 8] = [
        Self::Name,
        Self::Time,
        Self::Modified,
        Self::Size,
        Self::Shutter,
        Self::Quality,
        Self::Rating,
        Self::Edited,
    ];

    /// Whether this order needs the file's [`SortMeta`] read, so the Grid
    /// waits on the metadata jobs.
    ///
    /// [`SortMeta`]: crate::decode::image_decode::SortMeta
    pub(crate) fn reads_files(self) -> bool {
        matches!(
            self,
            Self::Time | Self::Modified | Self::Size | Self::Shutter
        )
    }

    /// Whether this order reads the stars or edit times the folder store holds,
    /// so a change to them reorders the Grid.
    pub(crate) fn reads_folder_store(self) -> bool {
        matches!(self, Self::Rating | Self::Edited)
    }

    /// The way a fresh pick of this key runs: oldest capture, fastest
    /// shutter and A to Z first; the most of anything else first.
    fn descends_by_default(self) -> bool {
        match self {
            Self::Name | Self::Time | Self::Shutter => false,
            Self::Modified | Self::Size | Self::Quality | Self::Rating | Self::Edited => true,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Time => "time",
            Self::Modified => "modified",
            Self::Size => "size",
            Self::Shutter => "shutter",
            Self::Quality => "quality",
            Self::Rating => "rating",
            Self::Edited => "edited",
        }
    }
}

/// The Grid's sort key and direction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub key: GridSort,
    pub descending: bool,
}

impl Default for SortOrder {
    fn default() -> SortOrder {
        SortOrder::by(GridSort::Name)
    }
}

impl SortOrder {
    const PREF_KEY: &'static str = "grid_sort";

    /// `key` in its natural direction.
    pub(crate) fn by(key: GridSort) -> SortOrder {
        SortOrder {
            key,
            descending: key.descends_by_default(),
        }
    }

    fn to_pref(self) -> String {
        let way = if self.descending { "desc" } else { "asc" };
        format!("{} {way}", self.key.as_str())
    }

    fn parse(s: &str) -> Option<SortOrder> {
        let (key, way) = s.trim().split_once(' ')?;
        let key = GridSort::ALL.into_iter().find(|k| k.as_str() == key)?;
        let descending = match way {
            "asc" => false,
            "desc" => true,
            _ => None?,
        };
        Some(SortOrder { key, descending })
    }

    /// The saved order, or Name when none is saved or it can't be read. The
    /// browser can't read file metadata, so a saved file sort gives Name
    /// there.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> SortOrder {
        crate::persist::prefs::load(SortOrder::PREF_KEY)
            .and_then(|s| SortOrder::parse(&s))
            .filter(|o| !(cfg!(target_arch = "wasm32") && o.key.reads_files()))
            .unwrap_or_default()
    }

    /// Remember the order for the next launch. A storage failure costs one
    /// relaunch's order, so it is logged, not surfaced.
    fn save(self) {
        // A test must never write the developer's own settings.
        if cfg!(test) {
            return;
        }
        if let Err(e) = crate::persist::prefs::save(SortOrder::PREF_KEY, &self.to_pref()) {
            eprintln!("[lightphotos] could not save the Grid sort: {e}");
        }
    }
}

impl App {
    pub(crate) fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    /// Sort the Grid by `key` in its natural direction. A key already in use
    /// keeps its direction.
    pub(super) fn set_sort(&mut self, key: GridSort) {
        if self.mode == ViewMode::Loupe || self.sort_order.key == key {
            return;
        }
        if key == GridSort::Quality && !self.quality_sort_available() {
            return;
        }
        self.apply_sort(SortOrder::by(key));
    }

    pub(super) fn reverse_sort(&mut self) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        let order = self.sort_order;
        self.apply_sort(SortOrder {
            descending: !order.descending,
            ..order
        });
    }

    pub(super) fn apply_sort(&mut self, order: SortOrder) {
        self.sort_order = order;
        order.save();
        self.recompute_visible();
        self.request_redraw();
    }

    /// A Quality sort restored at launch where no scoring pool started gives
    /// way to Name.
    pub(super) fn drop_unavailable_sort(&mut self) {
        if self.sort_order.key == GridSort::Quality && !self.quality_sort_available() {
            self.sort_order = SortOrder::default();
        }
    }

    /// Reorders the Grid after its stars or edit times changed. The Loupe
    /// keeps its order until the Grid shows again, so a photo rated there
    /// does not jump away from the cursor.
    pub(crate) fn resort_after_folder_store_change(&mut self) {
        if self.mode == ViewMode::Grid
            && self.sort_order.key.reads_folder_store()
            && self.sorted_revision != self.folder_store.sort_revision()
        {
            self.recompute_visible();
            self.request_redraw();
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::develop::Adjustments;
    use crate::navigation::Playlist;
    use std::path::PathBuf;

    fn folder_app(n: usize) -> (App, PathBuf, Vec<PathBuf>) {
        let names: Vec<String> = (0..n).map(|i| format!("p{i:02}.jpg")).collect();
        let (dir, paths) = crate::app::test_support::folder_of("sort-test", &names);
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        app.folder_store.open_dir(&dir);
        app.recompute_visible();
        (app, dir, paths)
    }

    fn visible_names(app: &App) -> Vec<String> {
        let Some(pl) = app.playlist.as_ref() else {
            return Vec::new();
        };
        app.visible
            .iter()
            .filter_map(|&i| pl.entry(i)?.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect()
    }

    fn edit(app: &mut App, path: &std::path::Path, exposure: f32) {
        let adj = Adjustments {
            exposure,
            ..Adjustments::default()
        };
        app.folder_store.set_adjustments(path, &adj);
    }

    #[test]
    fn the_rating_sort_puts_the_most_stars_first_and_reverses_to_the_fewest() {
        let (mut app, dir, paths) = folder_app(4);
        app.set_rating_of(paths[1].clone(), 2);
        app.set_rating_of(paths[2].clone(), 5);
        app.set_rating_of(paths[3].clone(), 2);
        app.set_sort(GridSort::Rating);
        assert_eq!(
            visible_names(&app),
            ["p02.jpg", "p01.jpg", "p03.jpg", "p00.jpg"],
            "most stars first, ties in name order, unrated as zero"
        );
        app.reverse_sort();
        assert_eq!(
            visible_names(&app),
            ["p00.jpg", "p01.jpg", "p03.jpg", "p02.jpg"],
            "unrated counts as a rating, so it leads the ascending sort"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_edited_sort_puts_the_unedited_last_whichever_way_it_runs() {
        let (mut app, dir, paths) = folder_app(4);
        edit(&mut app, &paths[1], 0.5);
        edit(&mut app, &paths[3], 0.5);
        app.folder_store.set_edited(&paths[1], 100);
        app.folder_store.set_edited(&paths[3], 300);
        app.set_sort(GridSort::Edited);
        assert_eq!(
            visible_names(&app),
            ["p03.jpg", "p01.jpg", "p00.jpg", "p02.jpg"],
            "newest edit first"
        );
        app.reverse_sort();
        assert_eq!(
            visible_names(&app),
            ["p01.jpg", "p03.jpg", "p00.jpg", "p02.jpg"],
            "the unedited stay last, in name order"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_descending_reverses_the_folder_order() {
        let (mut app, dir, _) = folder_app(3);
        app.reverse_sort();
        assert_eq!(visible_names(&app), ["p02.jpg", "p01.jpg", "p00.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picking_a_key_starts_it_in_its_natural_direction() {
        let (mut app, dir, _) = folder_app(1);
        app.reverse_sort();
        assert!(app.sort_order().descending);
        app.set_sort(GridSort::Rating);
        assert_eq!(app.sort_order(), SortOrder::by(GridSort::Rating));
        assert!(app.sort_order().descending);
        app.set_sort(GridSort::Time);
        assert!(!app.sort_order().descending);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expanded_stack_sorts_by_its_cover_and_stays_together() {
        let (mut app, dir, paths) = folder_app(5);
        crate::app::nav::tests::group_photos(&mut app, &[0, 2, 4], 2);
        let Some(id) = app.folder_store.groups().and_then(|g| {
            let name = paths[2].file_name()?;
            g.group_of(name).cloned()
        }) else {
            panic!("p00, p02 and p04 form a stack");
        };
        app.expanded_stacks.insert(id);
        app.set_rating_of(paths[0].clone(), 1);
        app.set_rating_of(paths[1].clone(), 3);
        app.set_rating_of(paths[2].clone(), 4);
        app.set_rating_of(paths[3].clone(), 5);
        app.set_sort(GridSort::Rating);
        assert_eq!(
            visible_names(&app),
            ["p03.jpg", "p00.jpg", "p02.jpg", "p04.jpg", "p01.jpg"],
            "the stack takes its cover's 4 stars and keeps its members in order"
        );
        app.reverse_sort();
        assert_eq!(
            visible_names(&app),
            ["p01.jpg", "p00.jpg", "p02.jpg", "p04.jpg", "p03.jpg"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rating_in_the_grid_reorders_it_but_the_loupe_waits() {
        let (mut app, dir, paths) = folder_app(3);
        app.set_sort(GridSort::Rating);
        app.set_rating_of(paths[2].clone(), 3);
        app.resort_after_folder_store_change();
        assert_eq!(visible_names(&app), ["p02.jpg", "p00.jpg", "p01.jpg"]);

        app.mode = ViewMode::Loupe;
        app.set_rating_of(paths[1].clone(), 4);
        app.resort_after_folder_store_change();
        assert_eq!(visible_names(&app), ["p02.jpg", "p00.jpg", "p01.jpg"]);
        app.mode = ViewMode::Grid;
        app.resort_after_folder_store_change();
        assert_eq!(visible_names(&app), ["p01.jpg", "p02.jpg", "p00.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rating_in_the_loupe_under_a_star_filter_keeps_the_photo_in_place() {
        let (mut app, dir, paths) = folder_app(4);
        for (p, stars) in paths.iter().zip([3, 2, 1, 0]) {
            app.set_rating_of(p.clone(), stars);
        }
        app.set_filter(Some((crate::navigation::Cmp::Gte, 1)));
        app.set_sort(GridSort::Rating);
        assert_eq!(visible_names(&app), ["p00.jpg", "p01.jpg", "p02.jpg"]);
        app.select_single(1);
        app.mode = ViewMode::Loupe;

        app.set_rating_of(paths[1].clone(), 5);
        assert_eq!(visible_names(&app), ["p00.jpg", "p01.jpg", "p02.jpg"]);
        app.step_loupe(true);
        assert_eq!(
            app.selected_path().as_ref(),
            Some(&paths[2]),
            "the old neighbour"
        );

        app.mode = ViewMode::Grid;
        app.resort_after_folder_store_change();
        assert_eq!(visible_names(&app), ["p01.jpg", "p00.jpg", "p02.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stack_whose_cover_is_filtered_out_sorts_by_its_first_member_left() {
        let (mut app, dir, paths) = folder_app(4);
        crate::app::nav::tests::group_photos(&mut app, &[0, 2], 0);
        let Some(id) = app.folder_store.groups().and_then(|g| {
            let name = paths[0].file_name()?;
            g.group_of(name).cloned()
        }) else {
            panic!("p00 and p02 form a stack");
        };
        app.expanded_stacks.insert(id);
        for (p, stars) in paths.iter().zip([1, 4, 5, 5]) {
            app.set_rating_of(p.clone(), stars);
        }
        app.set_filter(Some((crate::navigation::Cmp::Gte, 4)));
        app.set_sort(GridSort::Rating);
        assert_eq!(
            visible_names(&app),
            ["p02.jpg", "p03.jpg", "p01.jpg"],
            "p02 sorts with the 5 stars, not by its 1-star cover"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rating_sort_reads_the_stars_the_filter_reads() {
        let (mut app, dir, paths) = folder_app(3);
        app.ratings.insert(paths[2].clone(), 4);
        app.set_sort(GridSort::Rating);
        assert_eq!(visible_names(&app), ["p02.jpg", "p00.jpg", "p01.jpg"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_order_round_trips_and_a_bad_one_reads_as_nothing() {
        for key in GridSort::ALL {
            for descending in [false, true] {
                let order = SortOrder { key, descending };
                assert_eq!(SortOrder::parse(&order.to_pref()), Some(order));
            }
        }
        assert_eq!(
            SortOrder::parse("rating desc\n"),
            Some(SortOrder::by(GridSort::Rating))
        );
        for bad in [
            "",
            "rating",
            "rating up",
            "colour asc",
            "desc rating",
            "rating  desc",
        ] {
            assert_eq!(SortOrder::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_saved_quality_sort_gives_way_to_name_without_scoring() {
        let mut app = App::new(None);
        app.sort_order = SortOrder::by(GridSort::Quality);
        app.drop_unavailable_sort();
        assert_eq!(app.sort_order(), SortOrder::default());
    }
}
