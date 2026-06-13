//! egui UI. For T4 this is a trivial proof-of-integration panel; the real
//! grid / filmstrip / filter-bar / rating overlays land in T6.

/// Build the (currently trivial) egui UI for one frame. Called from inside
/// `egui::Context::run`.
///
// TODO: T6 — replace with grid, filmstrip, filter bar, and rating overlays.
pub fn debug_panel(ctx: &egui::Context) {
    egui::Window::new("Image Viewer")
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.label("egui ready — M1 UI coming");
        });
}
