// SPDX-License-Identifier: GPL-3.0-or-later

//! The Loupe's group pane: whether a grouped photo shows it, how sharp its
//! tiles are, and the member picked to become the representative.

use std::path::{Path, PathBuf};

use super::{App, SpikeTiles};
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

/// What a tile has of its member's full-resolution crop.
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

    /// The picked member, while the pane is open and it is still a member
    /// of the shown photo's group other than its representative. Showing
    /// another photo clears it (`load_selected`).
    pub(crate) fn group_pick(&self) -> Option<&Path> {
        let pick = self.pending_rep.as_deref()?;
        if self.group_view != GroupView::Compare {
            return None;
        }
        let (members, rep) = self.shown_group()?;
        let name = pick.file_name()?;
        (name != rep && members.iter().any(|m| m == name)).then_some(pick)
    }

    pub(super) fn set_group_view(&mut self, view: GroupView) {
        self.group_view = view;
        if view == GroupView::Edit {
            self.pending_rep = None;
        }
        self.request_redraw();
    }

    pub(super) fn set_tile_fidelity(&mut self, fidelity: TileFidelity) {
        self.tile_fidelity = fidelity;
        self.request_redraw();
    }

    /// Pick `path`, or clear the pick when `path` is already picked or is
    /// the representative.
    pub(super) fn pick_group_tile(&mut self, path: PathBuf) {
        let again = self.group_pick() == Some(path.as_path());
        self.pending_rep = Some(path);
        if again || self.group_pick().is_none() {
            self.pending_rep = None;
        }
        self.request_redraw();
    }

    pub(super) fn clear_group_pick(&mut self) {
        self.pending_rep = None;
    }

    /// Make the picked member the representative. Nothing without a pick.
    pub(super) fn set_pick_as_rep(&mut self) {
        let Some(path) = self.group_pick().map(Path::to_path_buf) else {
            return;
        };
        self.pending_rep = None;
        self.set_group_rep(&path);
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::nav::tests::{cells, group_photos};
    use crate::app::presets::tests::folder_app;
    use crate::ui::UiAction;

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

        act(&mut app, UiAction::SetGroupView(GroupView::Compare));
        assert_eq!(app.group_view(), GroupView::Compare);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ungrouped_photo_has_no_group_view_toggle() {
        let (mut app, dir, _) = grouped_loupe("group-view-single");
        act(&mut app, UiAction::Select(0));
        assert!(!app.shown_in_group());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tile_click_picks_without_changing_the_representative_until_set_as_rep() {
        let (mut app, dir, paths) = grouped_loupe("pick-then-set");
        act(&mut app, UiAction::SetGroupView(GroupView::Compare));

        act(&mut app, UiAction::PickGroupTile(paths[3].clone()));
        assert_eq!(app.group_pick(), Some(paths[3].as_path()));
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());
        assert_eq!(cells(&app), vec![0, 1, 2, 4, 5], "the cell keeps the rep");
        assert_eq!(
            app.want.as_ref(),
            Some(&paths[1]),
            "the Loupe keeps the rep"
        );

        act(&mut app, UiAction::SetPickAsRep);
        assert_eq!(rep_name(&app), paths[3].file_name().unwrap());
        assert_eq!(
            cells(&app),
            vec![0, 2, 3, 4, 5],
            "the cell shows the new rep"
        );
        assert_eq!(app.want.as_ref(), Some(&paths[3]), "the Loupe shows it");
        assert_eq!(app.group_pick(), None, "setting it spends the pick");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_as_rep_without_a_pick_or_with_the_rep_picked_changes_nothing() {
        let (mut app, dir, paths) = grouped_loupe("set-rep-no-pick");
        act(&mut app, UiAction::SetGroupView(GroupView::Compare));

        act(&mut app, UiAction::SetPickAsRep);
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());

        act(&mut app, UiAction::PickGroupTile(paths[1].clone()));
        assert_eq!(app.group_pick(), None, "the rep is not a pick");
        act(&mut app, UiAction::SetPickAsRep);
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());
        assert_eq!(app.want.as_ref(), Some(&paths[1]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clicking_the_picked_tile_again_clears_the_pick() {
        let (mut app, dir, paths) = grouped_loupe("pick-toggle");
        act(&mut app, UiAction::SetGroupView(GroupView::Compare));
        act(&mut app, UiAction::PickGroupTile(paths[3].clone()));
        act(&mut app, UiAction::PickGroupTile(paths[3].clone()));
        assert_eq!(app.group_pick(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pick_clears_when_the_loupe_moves_to_another_photo_or_leaves_compare() {
        let (mut app, dir, paths) = grouped_loupe("pick-clears");
        act(&mut app, UiAction::SetGroupView(GroupView::Compare));
        act(&mut app, UiAction::PickGroupTile(paths[3].clone()));

        act(&mut app, UiAction::Select(0));
        act(&mut app, UiAction::Select(1));
        assert_eq!(app.group_pick(), None, "another photo drops the pick");
        act(&mut app, UiAction::SetPickAsRep);
        assert_eq!(rep_name(&app), paths[1].file_name().unwrap());

        act(&mut app, UiAction::PickGroupTile(paths[3].clone()));
        act(&mut app, UiAction::SetGroupView(GroupView::Edit));
        act(&mut app, UiAction::SetGroupView(GroupView::Compare));
        assert_eq!(app.group_pick(), None, "leaving Compare drops the pick");
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
