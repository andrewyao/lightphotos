// SPDX-License-Identifier: GPL-3.0-or-later

//! The Loupe's group pane: whether a grouped photo shows it, how sharp its
//! tiles are, and the members picked to become the representative or to
//! be trashed.

use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use super::SpikeTiles;
use super::{App, PendingConfirm, ViewMode};
#[cfg(not(target_arch = "wasm32"))]
use crate::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
#[cfg(not(target_arch = "wasm32"))]
use crate::loader::Loader;
#[cfg(not(target_arch = "wasm32"))]
use crate::renderer::Renderer;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum GroupView {
    /// The Loupe as for a photo in no group.
    #[default]
    Edit,
    /// The group pane beside the photo.
    Compare,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TileFidelity {
    /// Each tile samples the member's preview.
    #[default]
    Speed,
    /// Each tile is cut from the member's full-resolution decode. Native
    /// only: the web build keeps one full decode in a 4 GB heap.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Full,
}

/// How a tile click changes the picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickHow {
    /// A plain click: pick only this member, or clear the picks when it is
    /// already the only one.
    Only,
    /// Cmd-click: add or remove this member.
    Toggle,
    /// Shift-click: pick every member from the anchor to this one in the
    /// pane's order, across pages.
    Range,
}

/// The members tile clicks picked, in the pane's order, and the member a
/// range extends from. Either can name a photo that has since left the
/// group, so `App::group_picks` filters on read.
#[derive(Debug, Default)]
pub(super) struct GroupPicks {
    picked: Vec<PathBuf>,
    anchor: Option<PathBuf>,
}

/// The zoom square every tile samples: its center in uv and its side as a
/// fraction of the photo's short side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Square {
    pub(crate) center: egui::Pos2,
    pub(crate) side: f32,
}

/// The square in uv of a `w`×`h` image, `side` of the short side across,
/// centered on `center` and kept inside the image.
pub(crate) fn spike_zoom_uv(w: u32, h: u32, center: egui::Pos2, side: f32) -> egui::Rect {
    let short = w.min(h) as f32;
    let half = egui::vec2(side * short / w as f32, side * short / h as f32) / 2.0;
    let c = egui::pos2(
        center.x.clamp(half.x, 1.0 - half.x),
        center.y.clamp(half.y, 1.0 - half.y),
    );
    egui::Rect::from_min_max(c - half, c + half)
}

/// The whole pixels `uv` covers in a `w`×`h` image, as (x, y, width,
/// height): at least one pixel each way and never past the edge.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(crate) fn crop_px(w: u32, h: u32, uv: egui::Rect) -> (u32, u32, u32, u32) {
    let px = |v: f32, n: u32| ((v.clamp(0.0, 1.0) * n as f32).round() as u32).min(n);
    let (x0, y0) = (px(uv.min.x, w).min(w - 1), px(uv.min.y, h).min(h - 1));
    let (x1, y1) = (px(uv.max.x, w).max(x0 + 1), px(uv.max.y, h).max(y0 + 1));
    (x0, y0, x1 - x0, y1 - y0)
}

/// What a tile has of its member's full-resolution crop. Only `None` on
/// the web, which has no Full.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum Full {
    None,
    /// The full decode is requested and not yet cut.
    Waiting,
    Ready {
        id: egui::TextureId,
        square: Square,
    },
    /// The decode came back with nothing for this square, so the tile stays
    /// on its preview rather than asking again every frame.
    Failed(Square),
}

/// One member on the pane's page: its preview texture and, in Full, the
/// crop of its full decode.
pub(crate) struct Tile {
    pub(crate) path: PathBuf,
    speed: egui::TextureId,
    w: u32,
    h: u32,
    full: Full,
}

impl Tile {
    pub(crate) fn new(path: PathBuf, speed: egui::TextureId, w: u32, h: u32) -> Self {
        Self {
            path,
            speed,
            w,
            h,
            full: Full::None,
        }
    }

    /// The texture to draw for `square` and the uv to sample it at: the crop
    /// when it was cut for this square, else the preview.
    pub(crate) fn texture(&self, square: Square) -> (egui::TextureId, egui::Rect) {
        match self.full {
            Full::Ready { id, square: s } if s == square => (
                id,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            ),
            _ => (
                self.speed,
                spike_zoom_uv(self.w, self.h, square.center, square.side),
            ),
        }
    }

    /// True while Full has no crop for `square` and may still get one.
    pub(crate) fn full_loading(&self, square: Square) -> bool {
        match self.full {
            Full::Ready { square: s, .. } | Full::Failed(s) => s != square,
            Full::None | Full::Waiting => true,
        }
    }

    pub(crate) fn textures(&self) -> impl Iterator<Item = egui::TextureId> {
        let crop = match self.full {
            Full::Ready { id, .. } => Some(id),
            _ => None,
        };
        std::iter::once(self.speed).chain(crop)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn drop_full(&mut self, r: &mut Renderer) {
        if let Full::Ready { id, .. } = self.full {
            r.free_thumb(id);
        }
        self.full = Full::None;
    }
}

/// How long the square has to stay put before Full cuts new crops, so a
/// drag doesn't queue a decode per frame.
#[cfg(not(target_arch = "wasm32"))]
const RECROP_AFTER: Duration = Duration::from_millis(300);

/// The loader keeps three full decodes and the Loupe's own zoom shares them,
/// so two in flight are never evicted before they are cut.
#[cfg(not(target_arch = "wasm32"))]
const FULL_IN_FLIGHT: usize = 2;

/// The pixels of `img` inside `(x, y, w, h)`.
#[cfg(not(target_arch = "wasm32"))]
fn crop_image(img: &DecodedImage, (x, y, w, h): (u32, u32, u32, u32)) -> DecodedImage {
    let bpp = match img.pixel_format {
        PixelFormat::Srgb8 => 4,
        PixelFormat::LinearF16 => 8,
    };
    let stride = img.width as usize * bpp;
    let row = w as usize * bpp;
    let mut rgba = Vec::with_capacity(row * h as usize);
    for r in y as usize..(y + h) as usize {
        let start = r * stride + x as usize * bpp;
        rgba.extend_from_slice(&img.rgba[start..start + row]);
    }
    DecodedImage::new_tracked(DecodedImageFields {
        width: w,
        height: h,
        rgba,
        pixel_format: img.pixel_format,
    })
}

/// Bring each Full tile on the page toward a crop of the current square:
/// cut any full decode that has landed, and once the square has settled,
/// request decodes for stale tiles, `FULL_IN_FLIGHT` at a time. Sets
/// `wake_at` while a re-crop waits on the square settling.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn sync_full_crops(
    tiles: &mut SpikeTiles,
    square: Square,
    loader: &mut Loader,
    r: &mut Renderer,
) {
    let now = Instant::now();
    if tiles.square != square {
        tiles.square = square;
        tiles.square_moved = now;
    }
    let settled = now.duration_since(tiles.square_moved) >= RECROP_AFTER;
    let mut in_flight = tiles
        .members
        .iter()
        .flatten()
        .filter(|t| matches!(t.full, Full::Waiting))
        .count();
    tiles.wake_at = None;
    for tile in tiles.members.iter_mut().flatten() {
        if tile.full_loading(square) && !matches!(tile.full, Full::Waiting) {
            if !settled {
                tiles.wake_at = Some(tiles.square_moved + RECROP_AFTER);
                continue;
            }
            if in_flight == FULL_IN_FLIGHT {
                continue;
            }
            tile.drop_full(r);
            loader.request_full(tile.path.clone());
            tile.full = Full::Waiting;
            in_flight += 1;
        }
        if !matches!(tile.full, Full::Waiting) {
            continue;
        }
        if let Some(img) = loader.get_full(&tile.path) {
            let uv = spike_zoom_uv(img.width, img.height, square.center, square.side);
            let crop = crop_image(&img, crop_px(img.width, img.height, uv));
            tile.full = match r.upload_image_spike(&crop, true) {
                Some(id) => Full::Ready { id, square },
                None => Full::Failed(square),
            };
            in_flight -= 1;
        } else if !loader.full_inflight(&tile.path) {
            tile.full = Full::Failed(square);
            in_flight -= 1;
        }
    }
}

/// Free every crop, for leaving Full.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn drop_full_crops(tiles: &mut SpikeTiles, r: &mut Renderer) {
    for tile in tiles.members.iter_mut().flatten() {
        tile.drop_full(r);
    }
    tiles.wake_at = None;
}

impl App {
    #[cfg(test)]
    pub(crate) fn group_view(&self) -> GroupView {
        self.group_view
    }

    pub(crate) fn tile_fidelity(&self) -> TileFidelity {
        self.tile_fidelity
    }

    pub(crate) fn spike_square(&self) -> Square {
        Square {
            center: self.spike_center,
            side: self.spike_side,
        }
    }

    /// The shown photo's group's members' names and its representative's.
    fn shown_group(&self) -> Option<(&[std::ffi::OsString], &std::ffi::OsString)> {
        let shown = self.selected_path()?;
        let groups = self.catalog.groups()?;
        let g = groups.get(groups.group_of(shown.file_name()?)?)?;
        Some((g.members(), g.rep()))
    }

    pub(crate) fn shown_in_group(&self) -> bool {
        self.shown_group().is_some()
    }

    /// `paths` in the pane's order: highest score first, unscored last, and
    /// ties in the order given.
    pub(super) fn by_score(&self, mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths.sort_by_key(|p| std::cmp::Reverse(self.catalog.score(p).map(|(s, _)| s.score.value)));
        paths
    }

    /// The members the pane shows, in its order: `by_score`, less those its
    /// flag filter hides.
    pub(super) fn compare_order(&self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut order = self.by_score(paths);
        let filter = self.compare_flag_filter;
        order.retain(|p| filter.matches(self.flag_of(p)));
        order
    }

    pub(crate) fn compare_flag_filter(&self) -> crate::navigation::FlagFilter {
        self.compare_flag_filter
    }

    pub(super) fn set_compare_flag_filter(&mut self, filter: crate::navigation::FlagFilter) {
        self.compare_flag_filter = filter;
        self.request_redraw();
    }

    /// The shown group's members in the pane's order (`by_score`) and its
    /// representative's path. `None` while the pane is closed.
    fn pane_members(&self) -> Option<(Vec<PathBuf>, PathBuf)> {
        if self.mode != ViewMode::Loupe || self.group_view != GroupView::Compare {
            return None;
        }
        let dir = self.playlist.as_ref()?.dir();
        let (members, rep) = self.shown_group()?;
        let members = members.iter().map(|m| dir.join(m)).collect();
        Some((self.compare_order(members), dir.join(rep)))
    }

    /// The members a pick may name: the shown group's, in the pane's order,
    /// other than the representative. `None` while the pane is closed.
    fn pickable(&self) -> Option<Vec<PathBuf>> {
        let (members, rep) = self.pane_members()?;
        Some(members.into_iter().filter(|m| *m != rep).collect())
    }

    /// A member's stars, for its tile.
    pub(crate) fn member_rating(&self, path: &Path) -> u8 {
        self.rating_of(path)
    }

    /// A member's stored score and whether it went stale, for its tile.
    pub(crate) fn member_score(
        &self,
        path: &Path,
    ) -> Option<(&crate::quality::QualityScore, bool)> {
        self.catalog.score(path).map(|(s, stale)| (&s.score, stale))
    }

    /// Rate a member of the shown group from its tile, the representative
    /// included, without touching the picks. Nothing for any other photo.
    pub(super) fn rate_group_member(&mut self, path: PathBuf, stars: u8) {
        if self
            .pane_members()
            .is_some_and(|(members, _)| members.contains(&path))
        {
            self.set_rating_of(path, stars);
        }
    }

    /// A member's flag, for its tile.
    pub(crate) fn member_flag(&self, path: &Path) -> Option<crate::catalog::Flag> {
        self.flag_of(path)
    }

    /// Flag a member of the shown group from its tile, as
    /// `rate_group_member` rates one.
    pub(super) fn flag_group_member(&mut self, path: PathBuf, flag: Option<crate::catalog::Flag>) {
        if self
            .pane_members()
            .is_some_and(|(members, _)| members.contains(&path))
        {
            self.set_flag_of(path, flag);
        }
    }

    /// The picked members that are still members of the shown photo's group
    /// other than its representative, in the pane's order. Empty while the
    /// pane is closed. Showing another photo clears them (`load_selected`).
    pub(crate) fn group_picks(&self) -> Vec<&Path> {
        let Some(pickable) = self.pickable() else {
            return Vec::new();
        };
        self.picks
            .picked
            .iter()
            .filter(|p| pickable.contains(p))
            .map(PathBuf::as_path)
            .collect()
    }

    pub(super) fn set_group_view(&mut self, view: GroupView) {
        self.group_view = view;
        if view == GroupView::Edit {
            self.clear_group_picks();
        }
        self.request_redraw();
    }

    pub(super) fn set_tile_fidelity(&mut self, fidelity: TileFidelity) {
        self.tile_fidelity = fidelity;
        self.request_redraw();
    }

    /// Change the picks for a click on `path`'s tile. A plain click on the
    /// representative clears them; a Cmd- or Shift-click on it does nothing.
    pub(super) fn pick_group_tile(&mut self, path: PathBuf, how: PickHow) {
        let Some(pickable) = self.pickable() else {
            return;
        };
        let Some(at) = pickable.iter().position(|p| *p == path) else {
            if how == PickHow::Only {
                self.clear_group_picks();
                self.request_redraw();
            }
            return;
        };
        let current: Vec<PathBuf> = self
            .group_picks()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        let anchor = self
            .picks
            .anchor
            .as_ref()
            .and_then(|a| pickable.iter().position(|p| p == a));
        let (picked, anchor) = match (how, anchor) {
            (PickHow::Only, _) if current == [path.clone()] => (Vec::new(), None),
            (PickHow::Only, _) | (PickHow::Range, None) => (vec![path.clone()], Some(path)),
            (PickHow::Toggle, _) => {
                let mut picked = current;
                match picked.iter().position(|p| *p == path) {
                    Some(i) => {
                        picked.remove(i);
                    }
                    None => picked.push(path.clone()),
                }
                (picked, Some(path))
            }
            (PickHow::Range, Some(a)) => {
                let range = pickable[a.min(at)..=a.max(at)].to_vec();
                (range, self.picks.anchor.clone())
            }
        };
        self.picks = GroupPicks {
            picked: pickable
                .into_iter()
                .filter(|p| picked.contains(p))
                .collect(),
            anchor,
        };
        self.request_redraw();
    }

    pub(super) fn clear_group_picks(&mut self) {
        self.picks = GroupPicks::default();
    }

    /// Make the picked member the representative. Nothing unless exactly
    /// one member is picked.
    /// Make `path`, a member the pane shows other than the representative,
    /// the representative, from its tile's hover button. Clears the picks.
    pub(super) fn set_member_as_rep(&mut self, path: PathBuf) {
        if !self.pickable().is_some_and(|m| m.contains(&path)) {
            return;
        }
        self.clear_group_picks();
        self.set_group_rep(&path);
        self.request_redraw();
    }

    /// Pick only `path`, a member the pane shows other than the
    /// representative, and open the confirm for trashing it, from its
    /// tile's Delete.
    pub(super) fn request_delete_member(&mut self, path: PathBuf) {
        if !self.pickable().is_some_and(|m| m.contains(&path)) {
            return;
        }
        self.picks = GroupPicks {
            picked: vec![path.clone()],
            anchor: Some(path),
        };
        self.request_delete_picks();
    }

    /// Open the confirm for trashing the picks, when there are any and no
    /// other delete is running.
    pub(super) fn request_delete_picks(&mut self) {
        if self.delete_available() && !self.group_picks().is_empty() {
            self.pending_confirm = Some(PendingConfirm::DeletePicks);
            self.request_redraw();
        }
    }

    /// Trash the picks, for the confirm's Delete.
    pub(super) fn delete_group_picks(&mut self) {
        let paths = self
            .group_picks()
            .into_iter()
            .map(Path::to_path_buf)
            .collect();
        self.clear_group_picks();
        self.start_delete(paths);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::nav::tests::{cells, group_photos};
    use crate::app::presets::tests::folder_app;
    use crate::ui::UiAction;
    use std::time::{Duration, Instant};

    /// Photos 1 and 3 grouped with 1 as representative, shown in the Loupe.
    fn grouped_loupe(name: &str) -> (App, PathBuf, Vec<PathBuf>) {
        let (mut app, dir, paths) = folder_app(name, 6);
        group_photos(&mut app, &[1, 3], 1);
        app.select_single(1);
        app.enter_loupe();
        (app, dir, paths)
    }

    fn act(app: &mut App, action: UiAction) {
        app.apply_ui_actions(vec![action]);
    }

    fn rep_name(app: &App) -> std::ffi::OsString {
        app.shown_group().unwrap().1.to_os_string()
    }

    #[test]
    fn a_grouped_photo_opens_in_edit_without_tiles_and_compare_turns_the_pane_on() {
        let (mut app, dir, _) = grouped_loupe("group-view");
        assert_eq!(app.group_view(), GroupView::Edit);
        assert!(app.shown_in_group(), "the Edit/Compare toggle shows");
        assert!(app.spike.is_none(), "Edit loads no tiles");

        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        assert_eq!(app.group_view(), GroupView::Compare);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compare_takes_develops_place_on_the_rail_and_needs_a_group() {
        use crate::app::{DevelopTab, RailItem};
        let (mut app, dir, _) = grouped_loupe("rail-compare");
        let group = || UiAction::ClickRail(RailItem::GroupCompare);
        let page = |tab| UiAction::ClickRail(RailItem::Develop(tab));

        act(&mut app, page(DevelopTab::Masks));
        act(&mut app, group());
        assert_eq!(app.group_view(), GroupView::Compare);
        assert_eq!(app.rail_lit(), Some(RailItem::GroupCompare));
        assert_eq!(app.develop_page_shown(), None, "Compare hides Develop");

        act(&mut app, group());
        assert_eq!(
            app.group_view(),
            GroupView::Edit,
            "its own icon turns it off"
        );
        assert_eq!(app.develop_page_shown(), Some(DevelopTab::Masks));

        act(&mut app, group());
        act(&mut app, page(DevelopTab::Sliders));
        assert_eq!(
            app.group_view(),
            GroupView::Edit,
            "a page icon turns it off"
        );
        assert_eq!(app.develop_page_shown(), Some(DevelopTab::Sliders));

        act(&mut app, group());
        act(&mut app, UiAction::Select(0));
        assert!(!app.shown_in_group());
        assert_eq!(
            app.group_view(),
            GroupView::Edit,
            "a lone photo leaves Compare"
        );
        assert_eq!(app.develop_page_shown(), Some(DevelopTab::Sliders));

        act(&mut app, group());
        assert_eq!(
            app.group_view(),
            GroupView::Edit,
            "no Compare outside a group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ungrouped_photo_has_no_group_view_toggle() {
        let (mut app, dir, _) = grouped_loupe("group-view-single");
        act(&mut app, UiAction::Select(0));
        assert!(!app.shown_in_group());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pick(app: &mut App, path: &Path, how: PickHow) {
        let path = path.to_path_buf();
        act(app, UiAction::PickGroupTile { path, how });
    }

    fn picks(app: &App) -> Vec<PathBuf> {
        app.group_picks()
            .into_iter()
            .map(Path::to_path_buf)
            .collect()
    }

    /// `members` of a `photos`-photo folder grouped under `rep`, shown in
    /// the Loupe with the Compare pane open.
    fn compare(
        name: &str,
        photos: usize,
        members: &[usize],
        rep: usize,
    ) -> (App, PathBuf, Vec<PathBuf>) {
        let (mut app, dir, paths) = folder_app(name, photos);
        group_photos(&mut app, members, rep);
        let pos = app.visible.iter().position(|&i| i == rep).unwrap();
        app.select_single(pos);
        app.enter_loupe();
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        (app, dir, paths)
    }

    /// The shown group's members in its own order.
    fn member_order(app: &App) -> Vec<PathBuf> {
        let dir = app.playlist.as_ref().unwrap().dir();
        app.shown_group()
            .unwrap()
            .0
            .iter()
            .map(|m| dir.join(m))
            .collect()
    }

    fn drain_delete(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            app.poll_delete();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn in_playlist(app: &App, path: &Path) -> bool {
        app.playlist
            .as_ref()
            .unwrap()
            .entries()
            .iter()
            .any(|p| p == path)
    }

    #[test]
    fn a_tile_click_picks_without_changing_the_representative_until_set_as_rep() {
        let (mut app, dir, paths) = grouped_loupe("pick-then-set");
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );

        pick(&mut app, &paths[3], PickHow::Only);
        assert_eq!(picks(&app), vec![paths[3].clone()]);
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());
        assert_eq!(cells(&app), vec![0, 1, 2, 4, 5], "the cell keeps the rep");
        assert_eq!(
            app.want.as_ref(),
            Some(&paths[1]),
            "the Loupe keeps the rep"
        );

        act(&mut app, UiAction::SetMemberAsRep(paths[3].clone()));
        assert_eq!(rep_name(&app), paths[3].file_name().unwrap());
        assert_eq!(
            cells(&app),
            vec![0, 2, 3, 4, 5],
            "the cell shows the new rep"
        );
        assert_eq!(app.want.as_ref(), Some(&paths[3]), "the Loupe shows it");
        assert!(picks(&app).is_empty(), "setting it spends the pick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_click_picks_only_that_tile_and_clicking_the_sole_pick_clears_it() {
        let (mut app, dir, paths) = compare("pick-only", 6, &[1, 2, 3, 4], 1);
        pick(&mut app, &paths[2], PickHow::Toggle);
        pick(&mut app, &paths[3], PickHow::Toggle);
        pick(&mut app, &paths[4], PickHow::Only);
        assert_eq!(picks(&app), vec![paths[4].clone()], "the others drop");
        pick(&mut app, &paths[4], PickHow::Only);
        assert!(picks(&app).is_empty(), "again clears it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cmd_click_toggles_a_tile_and_the_picks_keep_the_groups_order() {
        let (mut app, dir, paths) = compare("pick-toggle", 6, &[1, 2, 3, 4], 1);
        pick(&mut app, &paths[4], PickHow::Toggle);
        pick(&mut app, &paths[2], PickHow::Toggle);
        assert_eq!(picks(&app), vec![paths[2].clone(), paths[4].clone()]);
        pick(&mut app, &paths[4], PickHow::Toggle);
        assert_eq!(picks(&app), vec![paths[2].clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shift_click_picks_the_run_from_the_anchor_across_pages_and_skips_the_rep() {
        let (mut app, dir, _) = compare("pick-range", 14, &(0..12).collect::<Vec<_>>(), 4);
        let order = member_order(&app);
        assert!(
            order.len() > crate::app::SPIKE_PAGE,
            "the group spans pages"
        );
        let rep = app.want.clone().unwrap();
        let without_rep = |run: &[PathBuf]| -> Vec<PathBuf> {
            run.iter().filter(|p| **p != rep).cloned().collect()
        };

        pick(&mut app, &order[1], PickHow::Only);
        act(&mut app, UiAction::SpikePage(1));
        pick(&mut app, &order[11], PickHow::Range);
        assert!(order[1..=11].contains(&rep));
        assert_eq!(picks(&app), without_rep(&order[1..=11]));

        pick(&mut app, &order[0], PickHow::Range);
        assert_eq!(
            picks(&app),
            without_rep(&order[0..=1]),
            "a second Shift-click replaces the run from the same anchor"
        );

        pick(&mut app, &order[8], PickHow::Toggle);
        pick(&mut app, &order[6], PickHow::Range);
        assert_eq!(
            picks(&app),
            without_rep(&order[6..=8]),
            "Cmd-click moved the anchor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shift_click_with_no_anchor_picks_just_that_tile() {
        let (mut app, dir, paths) = compare("pick-range-first", 6, &[1, 2, 3, 4], 1);
        pick(&mut app, &paths[3], PickHow::Range);
        assert_eq!(picks(&app), vec![paths[3].clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_representative_is_never_picked() {
        let (mut app, dir, paths) = compare("pick-rep", 6, &[1, 2, 3], 1);
        pick(&mut app, &paths[2], PickHow::Only);
        pick(&mut app, &paths[1], PickHow::Toggle);
        pick(&mut app, &paths[1], PickHow::Range);
        assert_eq!(
            picks(&app),
            vec![paths[2].clone()],
            "Cmd and Shift on it do nothing"
        );
        pick(&mut app, &paths[1], PickHow::Only);
        assert!(
            picks(&app).is_empty(),
            "a plain click on it clears the picks"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_as_rep_names_a_shown_member_other_than_the_representative() {
        let (mut app, dir, paths) = compare("set-rep-one", 6, &[1, 2, 3], 1);
        act(&mut app, UiAction::SetMemberAsRep(paths[4].clone()));
        assert_eq!(
            rep_name(&app),
            paths[1].file_name().unwrap(),
            "not in the group"
        );

        pick(&mut app, &paths[2], PickHow::Toggle);
        act(&mut app, UiAction::SetMemberAsRep(paths[3].clone()));
        assert_eq!(
            rep_name(&app),
            paths[3].file_name().unwrap(),
            "no pick needed"
        );
        assert!(picks(&app).is_empty(), "and the picks clear");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_two_picks_asks_first_then_trashes_exactly_those_and_the_group_shrinks() {
        let (mut app, dir, paths) = compare("delete-picks", 6, &[1, 2, 3, 5], 1);
        pick(&mut app, &paths[2], PickHow::Only);
        pick(&mut app, &paths[3], PickHow::Range);
        act(&mut app, UiAction::RequestDeletePicks);
        let (kind, prompt) = app.pending_bulk_prompt().expect("the confirm opens");
        assert_eq!(kind, crate::ui::BulkKind::Delete);
        assert_eq!(prompt, (crate::i18n::t().confirm_delete)(2));
        assert!(
            paths.iter().all(|p| p.exists()),
            "nothing goes before the confirm"
        );

        act(&mut app, UiAction::ConfirmPending);
        drain_delete(&mut app);
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(exists, [true, true, false, false, true, true]);
        assert!(!in_playlist(&app, &paths[2]) && !in_playlist(&app, &paths[3]));
        assert_eq!(member_order(&app), vec![paths[1].clone(), paths[5].clone()]);
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());
        assert_eq!(
            app.want.as_ref(),
            Some(&paths[1]),
            "the Loupe keeps the rep"
        );
        assert!(picks(&app).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tiles_delete_asks_first_then_trashes_only_that_member() {
        let (mut app, dir, paths) = compare("delete-member", 6, &[1, 2, 3], 1);
        pick(&mut app, &paths[2], PickHow::Only);
        act(&mut app, UiAction::DeleteMember(paths[1].clone()));
        assert!(
            app.pending_bulk_prompt().is_none(),
            "never the representative"
        );

        act(&mut app, UiAction::DeleteMember(paths[3].clone()));
        let (_, prompt) = app.pending_bulk_prompt().expect("the confirm opens");
        assert_eq!(prompt, (crate::i18n::t().confirm_delete)(1));
        act(&mut app, UiAction::ConfirmPending);
        drain_delete(&mut app);
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(
            exists,
            [true, true, true, false, true, true],
            "not the old pick"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_every_member_but_the_rep_dissolves_the_group() {
        let (mut app, dir, paths) = compare("delete-picks-all", 4, &[1, 2], 1);
        pick(&mut app, &paths[2], PickHow::Only);
        act(&mut app, UiAction::RequestDeletePicks);
        act(&mut app, UiAction::ConfirmPending);
        drain_delete(&mut app);
        assert!(!paths[2].exists() && paths[1].exists());
        assert!(!app.shown_in_group());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_the_delete_trashes_nothing_and_keeps_the_picks() {
        let (mut app, dir, paths) = compare("delete-picks-cancel", 6, &[1, 2, 3], 1);
        pick(&mut app, &paths[2], PickHow::Toggle);
        pick(&mut app, &paths[3], PickHow::Toggle);
        act(&mut app, UiAction::RequestDeletePicks);
        act(&mut app, UiAction::CancelPending);
        assert!(app.pending_confirm.is_none());
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        assert_eq!(member_order(&app).len(), 3);
        assert_eq!(picks(&app), vec![paths[2].clone(), paths[3].clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_delete_key_with_picks_asks_to_trash_the_picks_not_the_shown_photo() {
        use winit::keyboard::KeyCode;
        let (mut app, dir, paths) = compare("delete-key-picks", 6, &[1, 2, 3], 1);
        app.handle_key(KeyCode::Delete);
        assert_eq!(
            app.pending_confirm,
            Some(PendingConfirm::Bulk(crate::ui::BulkKind::Delete)),
            "no picks, the shown photo's own Delete"
        );
        act(&mut app, UiAction::CancelPending);

        pick(&mut app, &paths[2], PickHow::Toggle);
        pick(&mut app, &paths[3], PickHow::Toggle);
        app.handle_key(KeyCode::Delete);
        assert_eq!(app.pending_confirm, Some(PendingConfirm::DeletePicks));
        let (_, prompt) = app.pending_bulk_prompt().unwrap();
        assert_eq!(prompt, (crate::i18n::t().confirm_delete)(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_toolbar_rates_the_picks_not_the_shown_photo() {
        let (mut app, dir, paths) = compare("rate-picks", 6, &[1, 2, 3, 4], 1);
        pick(&mut app, &paths[2], PickHow::Toggle);
        pick(&mut app, &paths[4], PickHow::Toggle);
        act(
            &mut app,
            UiAction::RequestBulk(crate::ui::BulkKind::Rate(3)),
        );
        let (_, prompt) = app.pending_bulk_prompt().expect("it asks first");
        assert_eq!(
            prompt,
            (crate::i18n::t().confirm_rate)("\u{2605}\u{2605}\u{2605}", 2)
        );
        act(&mut app, UiAction::ConfirmPending);
        let ratings: Vec<u8> = paths.iter().map(|p| app.member_rating(p)).collect();
        assert_eq!(ratings, [0, 0, 3, 0, 3, 0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_adjustment_with_one_pick_copies_from_that_pick() {
        let (mut app, dir, paths) = compare("copy-pick", 6, &[1, 2, 3], 1);
        pick(&mut app, &paths[3], PickHow::Only);
        act(&mut app, UiAction::CopySettings);
        assert_eq!(
            app.copied_settings.as_ref().map(|(p, _)| p),
            Some(&paths[3])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_picks_clear_when_the_loupe_moves_to_another_photo_or_leaves_compare() {
        let (mut app, dir, paths) = grouped_loupe("pick-clears");
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        pick(&mut app, &paths[3], PickHow::Only);

        act(&mut app, UiAction::Select(0));
        act(&mut app, UiAction::Select(1));
        assert!(picks(&app).is_empty(), "another photo drops the picks");

        pick(&mut app, &paths[3], PickHow::Only);
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        assert!(picks(&app).is_empty(), "leaving Compare drops the picks");

        pick(&mut app, &paths[3], PickHow::Only);
        app.mode = ViewMode::Grid;
        assert!(picks(&app).is_empty(), "the grid has no pane to pick in");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rate(app: &mut App, path: &Path, stars: u8) {
        let path = path.to_path_buf();
        act(app, UiAction::RateGroupMember { path, stars });
    }

    #[test]
    fn a_tiles_stars_rate_that_member_and_leave_the_shown_photo_and_picks_alone() {
        let (mut app, dir, paths) = compare("rate-member", 6, &[1, 2, 3], 1);
        pick(&mut app, &paths[2], PickHow::Only);
        rate(&mut app, &paths[3], 4);
        assert_eq!(app.member_rating(&paths[3]), 4);
        assert_eq!(app.selected_rating(), 0, "the shown rep is unrated");
        assert_eq!(picks(&app), vec![paths[2].clone()], "the picks stay");
        assert_eq!(app.want.as_ref(), Some(&paths[1]), "the Loupe stays put");

        rate(&mut app, &paths[1], 2);
        assert_eq!(app.selected_rating(), 2, "the rep's tile rates the rep");
        rate(&mut app, &paths[3], 0);
        assert_eq!(app.member_rating(&paths[3]), 0, "0 clears");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tile_rating_names_only_a_member_of_the_shown_group_in_compare() {
        let (mut app, dir, paths) = compare("rate-outsider", 6, &[1, 2, 3], 1);
        rate(&mut app, &paths[4], 3);
        assert_eq!(app.member_rating(&paths[4]), 0, "not in the group");
        act(
            &mut app,
            UiAction::ClickRail(crate::app::RailItem::GroupCompare),
        );
        rate(&mut app, &paths[2], 3);
        assert_eq!(app.member_rating(&paths[2]), 0, "the pane is closed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tiles_flags_flag_only_a_member_of_the_shown_group() {
        use crate::catalog::Flag;
        let (mut app, dir, paths) = compare("flag-member", 6, &[1, 2, 3], 1);
        let flag = |app: &mut App, path: &Path, flag| {
            let path = path.to_path_buf();
            act(app, UiAction::FlagGroupMember { path, flag });
        };
        flag(&mut app, &paths[3], Some(Flag::Pick));
        assert_eq!(app.member_flag(&paths[3]), Some(Flag::Pick));
        assert_eq!(app.want.as_ref(), Some(&paths[1]), "the Loupe stays put");
        flag(&mut app, &paths[3], None);
        assert_eq!(app.member_flag(&paths[3]), None, "None clears");
        flag(&mut app, &paths[4], Some(Flag::Pick));
        assert_eq!(app.member_flag(&paths[4]), None, "not in the group");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_panes_flag_filter_hides_rejected_tiles_by_default() {
        use crate::catalog::Flag;
        use crate::navigation::FlagFilter;
        let (mut app, dir, paths) = compare("flag-pane", 6, &[1, 2, 3], 1);
        let shown = |app: &App| app.pane_members().unwrap().0;
        assert_eq!(shown(&app).len(), 3);
        act(
            &mut app,
            UiAction::FlagGroupMember {
                path: paths[3].clone(),
                flag: Some(Flag::Reject),
            },
        );
        assert!(
            !shown(&app).contains(&paths[3]),
            "Not Rejected is the default"
        );
        act(
            &mut app,
            UiAction::SetCompareFlagFilter(FlagFilter::Rejected),
        );
        assert_eq!(shown(&app), vec![paths[3].clone()]);
        assert_eq!(
            app.flag_filter(),
            FlagFilter::All,
            "the grid keeps its own filter"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_score(app: &mut App, path: &Path, value: u8) {
        let score = crate::quality::QualityScore {
            value,
            basis: crate::quality::Basis::TechnicalOnly,
            base: None,
            deductions: Vec::new(),
        };
        app.catalog.set_score(path, score, 0);
    }

    #[test]
    fn the_pane_orders_members_highest_score_first_and_unscored_last() {
        let (mut app, dir, paths) = compare("pane-order", 6, &[1, 2, 3, 4], 1);
        set_score(&mut app, &paths[2], 40);
        set_score(&mut app, &paths[4], 80);
        set_score(&mut app, &paths[1], 40);
        let (order, _) = app.pane_members().unwrap();
        assert_eq!(
            order,
            [&paths[4], &paths[1], &paths[2], &paths[3]].map(|p| p.clone()),
            "a tie keeps the group's order"
        );

        pick(&mut app, &paths[4], PickHow::Only);
        pick(&mut app, &paths[2], PickHow::Range);
        assert_eq!(
            picks(&app),
            vec![paths[4].clone(), paths[2].clone()],
            "Shift-click runs in the pane's order and skips the rep"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouping_makes_the_first_highest_scored_photo_the_representative() {
        let (mut app, dir, paths) = folder_app("group-rep-score", 6);
        set_score(&mut app, &paths[1], 50);
        set_score(&mut app, &paths[2], 70);
        set_score(&mut app, &paths[4], 70);
        app.select_single(1);
        for pos in [2, 3, 4] {
            act(&mut app, UiAction::SelectToggle(pos));
        }
        app.group_selected();
        let groups = app.catalog.groups().unwrap();
        let g = groups
            .get(groups.group_of(paths[1].file_name().unwrap()).unwrap())
            .unwrap();
        assert_eq!(g.rep(), paths[2].file_name().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouping_unscored_photos_keeps_the_cursor_as_representative() {
        let (mut app, dir, paths) = folder_app("group-rep-cursor", 6);
        app.select_single(1);
        act(&mut app, UiAction::SelectToggle(3));
        act(&mut app, UiAction::SelectToggle(2));
        app.group_selected();
        let groups = app.catalog.groups().unwrap();
        let g = groups
            .get(groups.group_of(paths[1].file_name().unwrap()).unwrap())
            .unwrap();
        let cursor = app
            .selected_path()
            .and_then(|p| p.file_name().map(|n| n.to_os_string()));
        assert_eq!(Some(g.rep().to_os_string()), cursor);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crop_px_covers_the_uv_in_whole_pixels_inside_the_image() {
        let r = |x0, y0, x1, y1| egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1));
        assert_eq!(
            crop_px(4000, 3000, r(0.25, 0.5, 0.5, 1.0)),
            (1000, 1500, 1000, 1500)
        );
        assert_eq!(crop_px(100, 100, r(-0.2, -0.1, 1.3, 1.2)), (0, 0, 100, 100));
        assert_eq!(crop_px(100, 100, r(1.0, 1.0, 1.0, 1.0)), (99, 99, 1, 1));
        assert_eq!(crop_px(10, 10, r(0.31, 0.31, 0.31, 0.31)), (3, 3, 1, 1));
    }
}
