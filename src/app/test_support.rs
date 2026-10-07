//! Folders and Apps for tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{App, ViewMode};
use crate::navigation::Playlist;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// An empty folder under the system temp dir, new on every call.
pub(crate) fn temp_folder(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lightphotos-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `temp_folder` holding an empty file for each of `names`.
pub(crate) fn folder_of(tag: &str, names: &[String]) -> (PathBuf, Vec<PathBuf>) {
    let dir = temp_folder(tag);
    let paths = names
        .iter()
        .map(|n| {
            let p = dir.join(n);
            std::fs::write(&p, []).unwrap();
            p
        })
        .collect();
    (dir, paths)
}

/// An App in the Grid over a folder of `photos` empty files named `0.jpg`,
/// `1.jpg` and on, with the folder's sidecars read before it returns.
pub(crate) fn folder_app(tag: &str, photos: usize) -> (App, PathBuf, Vec<PathBuf>) {
    let names: Vec<String> = (0..photos).map(|i| format!("{i}.jpg")).collect();
    let (dir, paths) = folder_of(tag, &names);
    let mut app = App::new(None);
    app.catalog.open_dir(&dir);
    app.playlist = Some(Playlist::from_dir(&dir));
    app.mode = ViewMode::Grid;
    app.recompute_visible();
    (app, dir, paths)
}

/// Waits for the catalog load `App::load_playlist` started, failing after
/// `limit` rather than hanging the suite.
pub(crate) fn wait_for_catalog_within(app: &mut App, limit: Duration) {
    let deadline = Instant::now() + limit;
    while app.poll_catalog_load() {
        assert!(Instant::now() < deadline, "catalog load timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn wait_for_catalog(app: &mut App) {
    wait_for_catalog_within(app, Duration::from_secs(10));
}

/// `App::load_playlist` on `dir`, then `wait_for_catalog`.
pub(crate) fn load_folder(app: &mut App, dir: &Path) {
    app.load_playlist(Playlist::from_dir(dir), dir.to_path_buf());
    wait_for_catalog(app);
}

/// One real frame of the whole UI. Returns the actions it pushed and every
/// string it painted with where it landed, so a test can aim a click at a
/// widget it cannot see.
pub(crate) fn frame(
    app: &mut App,
    events: Vec<egui::Event>,
) -> (Vec<crate::ui::UiAction>, Painted) {
    frame_with_modifiers(app, events, egui::Modifiers::NONE)
}

/// A frame with `modifiers` held down.
pub(crate) fn frame_with_modifiers(
    app: &mut App,
    events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
) -> (Vec<crate::ui::UiAction>, Painted) {
    frame_sized(app, events, modifiers, egui::vec2(1100.0, 800.0))
}

/// What the UI paints once settled in a window `size` points big.
pub(crate) fn settled_at(app: &mut App, size: egui::Vec2) -> Painted {
    let none = egui::Modifiers::NONE;
    let _ = frame_sized(app, Vec::new(), none, size);
    frame_sized(app, Vec::new(), none, size).1
}

fn frame_sized(
    app: &mut App,
    events: Vec<egui::Event>,
    modifiers: egui::Modifiers,
    size: egui::Vec2,
) -> (Vec<crate::ui::UiAction>, Painted) {
    let ctx = app.egui_ctx.clone();
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
        events,
        modifiers,
        ..Default::default()
    };
    let mut actions = Vec::new();
    let output = ctx.run_ui(input, |ui| {
        actions = crate::ui::draw(ui, app).actions;
    });
    let mut texts = Vec::new();
    let mut circles = Vec::new();
    let mut rects = Vec::new();
    for clipped in &output.shapes {
        match &clipped.shape {
            egui::Shape::Text(text) => texts.push((
                text.galley.text().to_string(),
                text.pos + egui::vec2(4.0, text.galley.size().y / 2.0),
            )),
            egui::Shape::Circle(circle) => circles.push(*circle),
            egui::Shape::Rect(rect) => rects.push((rect.rect, rect.stroke.color)),
            _ => {}
        }
    }
    (actions, Painted(texts, circles, rects))
}

pub(crate) struct Painted(
    Vec<(String, egui::Pos2)>,
    Vec<egui::epaint::CircleShape>,
    Vec<(egui::Rect, egui::Color32)>,
);

impl Painted {
    pub(crate) fn has(&self, text: &str) -> bool {
        self.0.iter().any(|(t, _)| t == text)
    }

    pub(crate) fn any_containing(&self, needle: &str) -> bool {
        self.0.iter().any(|(t, _)| t.contains(needle))
    }

    pub(crate) fn pos_of(&self, text: &str) -> egui::Pos2 {
        self.0
            .iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| panic!("nothing painted {text:?}; got {:?}", self.texts()))
            .1
    }

    /// The `text` nearest `anchor`, for a label the window paints more than
    /// once. Touch Up has its own Delete button, so the row menu's has to be
    /// picked by where it opened.
    pub(crate) fn pos_of_near(&self, text: &str, anchor: egui::Pos2) -> egui::Pos2 {
        self.0
            .iter()
            .filter(|(t, _)| t == text)
            .min_by(|(_, a), (_, b)| a.distance(anchor).total_cmp(&b.distance(anchor)))
            .unwrap_or_else(|| panic!("nothing painted {text:?}; got {:?}", self.texts()))
            .1
    }

    pub(crate) fn texts(&self) -> Vec<&str> {
        self.0.iter().map(|(t, _)| t.as_str()).collect()
    }

    /// Every rect outlined in `stroke`, at any opacity, since a modal
    /// fading in paints everything in it translucent.
    pub(crate) fn outlined(&self, stroke: egui::Color32) -> Vec<egui::Rect> {
        let near = |c: egui::Color32| {
            let (a, b) = (c.to_opaque().to_array(), stroke.to_array());
            a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 2)
        };
        self.2
            .iter()
            .filter(|(_, color)| color.a() > 0 && near(*color))
            .map(|(rect, _)| *rect)
            .collect()
    }

    /// Every circle painted in `fill`.
    pub(crate) fn circles_filled(&self, fill: egui::Color32) -> Vec<egui::Pos2> {
        self.1
            .iter()
            .filter(|c| c.fill == fill)
            .map(|c| c.center)
            .collect()
    }
}

/// What the UI paints once it has settled. A modal is an `egui::Area`,
/// which egui sizes on one frame and paints on the next, so one frame is
/// not enough to see one.
pub(crate) fn settled(app: &mut App) -> Painted {
    let _ = frame(app, Vec::new());
    frame(app, Vec::new()).1
}

/// Presses at `pos` in one frame and releases in the next, which is when
/// egui reports the click. Returns that frame's actions, and what the UI
/// paints once it has settled afterwards.
pub(crate) fn click(app: &mut App, pos: egui::Pos2) -> (Vec<crate::ui::UiAction>, Painted) {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    let _ = frame(app, vec![egui::Event::PointerMoved(pos), button(true)]);
    let (actions, _) = frame(app, vec![button(false)]);
    (actions, settled(app))
}
