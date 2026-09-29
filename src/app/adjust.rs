use super::*;
use std::path::Path;

use crate::develop::{self, Adjustments, GpuAdjust, GpuTouchUp, TouchUp};
use crate::image_ops;

impl App {
    /// Develop adjustments of the image currently shown (identity if unset).
    pub(crate) fn current_adjustments(&self) -> Adjustments {
        self.shown
            .path()
            .and_then(|p| self.edits.get(p))
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn current_touchups(&self) -> &[TouchUp] {
        self.shown
            .path()
            .and_then(|p| self.touchups.get(p))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Switch the Develop panel's tab. Touch Up's controls live on Masks, so
    /// leaving it disarms the tool rather than leave it armed out of sight.
    pub(crate) fn set_develop_tab(&mut self, tab: DevelopTab) {
        if tab != DevelopTab::Masks && self.tool == LoupeTool::TouchUp {
            self.tool = LoupeTool::None;
            self.touchup_selected = None;
        }
        self.develop_tab = tab;
        self.request_redraw();
    }

    pub(crate) fn touchup_active(&self) -> bool {
        self.tool == LoupeTool::TouchUp
    }
    pub(crate) fn toggle_touchup(&mut self) {
        self.tool = if self.tool == LoupeTool::TouchUp {
            LoupeTool::None
        } else {
            LoupeTool::TouchUp
        };
        self.request_redraw();
    }
    pub(crate) fn touchup_radius(&self) -> f32 {
        self.touchup_radius.max(self.touchup_radius_min())
    }
    pub(crate) fn touchup_selected(&self) -> Option<usize> {
        self.touchup_selected
    }
    pub(crate) fn touchup_spots_shown(&self) -> bool {
        !self.touchup_spots_hidden
    }
    pub(super) fn toggle_touchup_spots(&mut self) {
        self.touchup_spots_hidden = !self.touchup_spots_hidden;
        self.request_redraw();
    }
    /// Select the spot after the selected one, wrapping, and show the spots so
    /// the selection can be seen.
    pub(super) fn select_next_touchup(&mut self) {
        let n = self.current_touchups().len();
        if n == 0 {
            return;
        }
        self.touchup_selected = Some(self.touchup_selected.map_or(0, |i| (i + 1) % n));
        self.touchup_spots_hidden = false;
        self.request_redraw();
    }
    pub(crate) fn set_touchup_radius(&mut self, radius: f32) {
        self.touchup_radius = radius.clamp(self.touchup_radius_min(), TOUCHUP_MAX_RADIUS);
    }
    /// Multiplicative, so each press is the same visible step at any size.
    pub(crate) fn step_touchup_radius(&mut self, grow: bool) {
        let factor = if grow { 1.15 } else { 1.0 / 1.15 };
        self.set_touchup_radius(self.touchup_radius() * factor);
        self.request_redraw();
    }
    pub(crate) fn touchup_feather(&self) -> f32 {
        self.touchup_feather
    }
    pub(crate) fn set_touchup_feather(&mut self, feather: f32) {
        self.touchup_feather = feather.clamp(TOUCHUP_MIN_FEATHER, 1.0);
    }
    pub(crate) fn step_touchup_feather(&mut self, delta: f32) {
        self.set_touchup_feather(self.touchup_feather + delta);
        self.request_redraw();
    }
    pub(crate) fn touchup_radius_min(&self) -> f32 {
        let (w, h) = self.image_size();
        (TOUCHUP_MIN_PIXELS / w.min(h)).min(TOUCHUP_MAX_RADIUS)
    }

    /// Touch-up radius in normalized UV units for each image axis. The stored
    /// radius is relative to the source image's shorter dimension.
    pub(crate) fn touchup_uv_radii(&self, radius: f32) -> (f32, f32) {
        let (w, h) = self.image_size();
        let min_dim = w.min(h);
        (radius * min_dim / w, radius * min_dim / h)
    }

    /// Save `adj` for the shown image and push it to the GPU. Identity edits are
    /// removed from the edits map rather than stored.
    pub(super) fn apply_adjustments(&mut self, adj: Adjustments) {
        self.apply_adjustments_kind(adj, "adjustment");
    }

    pub(super) fn apply_adjustments_kind(&mut self, adj: Adjustments, _kind: &'static str) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        // Nothing to record, but the GPU can still hold a preview value that
        // differs from the stored edit: crop mode pushes `crop: None` so the
        // full frame shows under the overlay, and committing an unchanged crop
        // lands here. Resync before leaving, or the loupe keeps the preview.
        if self.current_adjustments() == adj {
            self.push_adjustments();
            return;
        }
        if adj.is_identity() {
            self.edits.remove(&path);
        } else {
            self.edits.insert(path.clone(), adj);
        }
        if self.unsaved_edit.as_ref().is_some_and(|p| *p != path) {
            self.save_edit();
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.unsaved_edit_kind = _kind;
        }
        self.unsaved_edit = Some(path);
        self.save_edit_unless_dragging();
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    /// A slider drag changes the edit every frame. Writing the sidecar each
    /// time blocks the UI thread on disk I/O, so wait for the mouse release.
    pub(crate) fn save_edit_unless_dragging(&mut self) {
        if !self.egui_ctx.input(|i| i.pointer.any_down()) {
            self.save_edit();
        }
    }

    pub(crate) fn save_edit(&mut self) {
        if let Some(path) = self.unsaved_edit.take() {
            let adj = self.edits.get(&path).copied().unwrap_or_default();
            self.catalog.set_adjustments(&path, &adj);
            #[cfg(target_arch = "wasm32")]
            crate::analytics::property("develop_edit_applied", "edit_kind", self.unsaved_edit_kind);
        }
    }

    /// Push the current image's adjustments to the renderer. Call it whenever
    /// the shown image or its edits change.
    pub(super) fn push_adjustments(&mut self) {
        #[cfg(test)]
        {
            self.pushed_adj = Some(self.current_adjustments());
        }
        let gpu = self.gpu_adjust(&self.current_adjustments());
        let gpu_touchups: Vec<GpuTouchUp> = self
            .current_touchups()
            .iter()
            .map(GpuTouchUp::from)
            .collect();
        if let Some(r) = &mut self.renderer {
            r.set_adjustments(gpu);
            r.set_touchups(&gpu_touchups);
        }
        self.request_redraw();
    }

    /// Convert `adj` to the GPU uniform. Also fills `texel_w`/`texel_h` from the
    /// image size, which denoise needs to step by whole texels.
    pub(super) fn gpu_adjust(&self, adj: &Adjustments) -> GpuAdjust {
        let (w, h) = self.image_size();
        let mut g = GpuAdjust::from(adj);
        g.texel_w = 1.0 / w;
        g.texel_h = 1.0 / h;
        g._pad0 = self.current_touchups().len() as f32;
        g
    }

    pub(super) fn apply_touchups(&mut self, touchups: Vec<TouchUp>) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if touchups.len() > crate::develop::MAX_TOUCHUPS {
            self.set_status(StatusKind::Error, crate::i18n::t().touch_up_limit.into());
            return;
        }
        #[cfg(target_arch = "wasm32")]
        let changed = self.current_touchups() != touchups.as_slice();
        if touchups.is_empty() {
            self.touchups.remove(&path);
        } else {
            self.touchups.insert(path.clone(), touchups.clone());
        }
        self.catalog.set_touchups(&path, &touchups);
        #[cfg(target_arch = "wasm32")]
        if changed {
            crate::analytics::property("develop_edit_applied", "edit_kind", "touch_up");
        }
        self.touchup_selected = None;
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    pub(super) fn choose_touchup(&self, u: f32, v: f32) -> Option<TouchUp> {
        let path = self.shown.path()?;
        // Samples by UV, so whichever loupe tier has landed will do.
        let img = self.loader.as_ref()?.get_best(path, self.preview_px())?;
        let radius = self.touchup_radius();
        let (radius_u, radius_v) = self.touchup_uv_radii(radius);
        let sample = |u: f32, v: f32| {
            let x = (u.clamp(0.0, 1.0) * (img.width.saturating_sub(1)) as f32).round() as u32;
            let y = (v.clamp(0.0, 1.0) * (img.height.saturating_sub(1)) as f32).round() as u32;
            image_ops::sample_linear(
                &img,
                x as f32 / img.width.saturating_sub(1).max(1) as f32,
                y as f32 / img.height.saturating_sub(1).max(1) as f32,
            )
        };
        // Compare colors on the patch boundary itself. A ring outside the
        // patch leaves a color step at the edge that shows as a halo.
        let ring = |cx: f32, cy: f32| -> [f32; 3] {
            let mut sum = [0.0; 3];
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let p = sample(cx + a.cos() * radius_u, cy + a.sin() * radius_v);
                for c in 0..3 {
                    sum[c] += p[c];
                }
            }
            for c in 0..3 {
                sum[c] /= 8.0;
            }
            sum
        };
        let target_ring = ring(u, v);
        let (unit_radius_u, unit_radius_v) = self.touchup_uv_radii(1.0);
        let mut best: Option<(f32, f32, f32)> = None;
        for i in 0..16 {
            let a = i as f32 * std::f32::consts::TAU / 16.0;
            let distance = radius * (3.0 + (i % 3) as f32);
            let su = u + a.cos() * distance * unit_radius_u;
            let sv = v + a.sin() * distance * unit_radius_v;
            if su < radius_u || su > 1.0 - radius_u || sv < radius_v || sv > 1.0 - radius_v {
                continue;
            }
            let sr = ring(su, sv);
            let score = (0..3)
                .map(|c| (sr[c] - target_ring[c]).powi(2))
                .sum::<f32>()
                + distance * 0.002;
            if best.map_or(true, |(s, _, _)| score < s) {
                best = Some((score, su, sv));
            }
        }
        let (_, su, sv) = best?;
        let source_ring = ring(su, sv);
        Some(TouchUp {
            center: [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)],
            radius,
            source: [su, sv],
            feather: self.touchup_feather,
            delta: [
                target_ring[0] - source_ring[0],
                target_ring[1] - source_ring[1],
                target_ring[2] - source_ring[2],
            ],
        })
    }

    pub(super) fn add_touchup(&mut self, u: f32, v: f32) {
        let Some(t) = self.choose_touchup(u, v) else {
            self.set_status(
                StatusKind::Error,
                crate::i18n::t().touch_up_needs_full.into(),
            );
            return;
        };
        let mut all = self.current_touchups().to_vec();
        all.push(t);
        let new_index = all.len() - 1;
        if self.edit_touchups(all) {
            self.touchup_selected = Some(new_index);
        }
    }

    pub(super) fn delete_selected_touchup(&mut self) {
        let Some(i) = self.touchup_selected else {
            return;
        };
        let mut all = self.current_touchups().to_vec();
        if i < all.len() {
            all.remove(i);
            self.edit_touchups(all);
        }
    }

    /// Applies a Touch Up add or delete, remembering the spots it replaced for
    /// Undo. False when the change was refused (the 64-spot limit).
    fn edit_touchups(&mut self, touchups: Vec<TouchUp>) -> bool {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return false;
        };
        let before = self.current_touchups().to_vec();
        self.apply_touchups(touchups);
        if self.current_touchups() == before.as_slice() {
            return false;
        }
        self.touchup_undo.entry(path).or_default().push(before);
        true
    }

    /// Steps the shown image's spots back to before the last add or delete.
    pub(super) fn undo_touchup(&mut self) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(before) = self.touchup_undo.get_mut(&path).and_then(Vec::pop) else {
            return;
        };
        self.apply_touchups(before);
    }

    /// True while the next Loupe click samples a pixel for white balance.
    pub(crate) fn wb_picker_active(&self) -> bool {
        self.tool == LoupeTool::WbPicker
    }

    pub(super) fn toggle_wb_picker(&mut self) {
        if self.tool == LoupeTool::WbPicker {
            self.tool = LoupeTool::None;
        } else {
            self.tool = LoupeTool::WbPicker;
            self.touchup_selected = None;
        }
        self.request_redraw();
    }

    /// Set temp/tint so the histogram-grid pixel at UV `(u, v)` becomes neutral
    /// gray. Always leaves picker mode, even when the pixel is too dark to use.
    pub(super) fn pick_white_balance(&mut self, u: f32, v: f32) {
        self.tool = LoupeTool::None;
        self.request_redraw();
        if self.hist_dw == 0 || self.hist_dh == 0 {
            self.set_status(StatusKind::Error, crate::i18n::t().wb_no_image.into());
            return;
        }
        let gx = ((u * self.hist_dw as f32) as usize).min(self.hist_dw - 1);
        let gy = ((v * self.hist_dh as f32) as usize).min(self.hist_dh - 1);
        let px = self.hist_sample[gy * self.hist_dw + gx];
        match develop::neutralize_gray(px) {
            Some((temp, tint)) => {
                let mut adj = self.current_adjustments();
                adj.temp = temp;
                adj.tint = tint;
                self.apply_adjustments(adj);
            }
            None => {
                self.set_status(StatusKind::Error, crate::i18n::t().wb_pick_brighter.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &App, down: bool) {
        let pos = egui::pos2(1.0, 1.0);
        let input = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: down,
                    modifiers: Default::default(),
                },
            ],
            ..Default::default()
        };
        let _ = app.egui_ctx.run_ui(input, |_| {});
    }

    fn one_photo(tag: &str) -> (App, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lp-adjust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        let mut app = App::new(None);
        app.catalog.open_dir(&dir);
        app.shown = Shown::Preview(photo.clone(), 1024, 1024);
        (app, dir, photo)
    }

    #[test]
    fn showing_fractional_catalog_edits_does_not_emit_an_edit_action() {
        let (mut app, dir, photo) = one_photo("display-only");
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;
        app.develop_open = true;
        let adj = Adjustments {
            exposure: 0.12345,
            contrast: 12.345,
            ..Default::default()
        };
        app.edits.insert(photo, adj);
        let ctx = app.egui_ctx.clone();
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let out = crate::ui::draw(ui, &mut app);
                assert!(!out
                    .actions
                    .iter()
                    .any(|action| matches!(action, crate::ui::UiAction::SetAdjustments(_))));
            });
        }
        assert_eq!(app.current_adjustments(), adj);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_slider_drag_writes_the_sidecar_once_on_release() {
        let (mut app, dir, photo) = one_photo("drag");
        let sidecar = dir.join(".lightphotos").join("a.jpg.xmp");
        press(&app, true);
        for step in 1..=5 {
            app.apply_adjustments(Adjustments {
                exposure: step as f32 * 0.1,
                ..Default::default()
            });
        }
        assert!(
            !sidecar.exists(),
            "nothing is written while the mouse is held"
        );

        press(&app, false);
        app.save_edit_unless_dragging();
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(sidecar.exists(), "the release writes the edit");
        assert_eq!(app.catalog.adjustments(&photo).exposure, 0.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_keyboard_nudge_is_written_at_once() {
        let (mut app, dir, _) = one_photo("key");
        app.apply_adjustments(Adjustments {
            contrast: 10.0,
            ..Default::default()
        });
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(dir.join(".lightphotos").join("a.jpg.xmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Develop panel's tabs and Touch Up's brush, driven through the real
    /// widget tree with the presets module's pointer harness.
    mod develop_tabs {
        use super::super::*;
        use crate::app::presets::tests::{click, folder_app, frame, frame_with_modifiers, settled};
        use crate::ui::UiAction;

        fn loupe(tag: &str) -> App {
            let (mut app, _dir, paths) = folder_app(tag, 1);
            app.mode = ViewMode::Loupe;
            app.shown = Shown::Preview(paths[0].clone(), 4000, 3000);
            app.source_size = Some((4000, 3000));
            app.develop_open = true;
            app
        }

        #[test]
        fn the_panel_opens_on_sliders_and_touch_up_lives_on_masks() {
            let mut app = loupe("tabs");
            let t = crate::i18n::t();
            let painted = settled(&mut app);
            assert!(painted.has(t.tab_sliders) && painted.has(t.tab_masks));
            assert!(
                !painted.has(t.touch_up),
                "Touch Up is not on the Sliders tab: {:?}",
                painted.texts()
            );
            assert_eq!(painted.has(t.tab_crop), SHOW_CROP_TAB);

            let (actions, _) = click(&mut app, painted.pos_of(t.tab_masks));
            let tab = actions.into_iter().find_map(|a| match a {
                UiAction::SetDevelopTab(tab) => Some(tab),
                _ => None,
            });
            assert_eq!(tab, Some(DevelopTab::Masks));
            app.set_develop_tab(DevelopTab::Masks);
            assert!(settled(&mut app).has(t.touch_up));
        }

        #[test]
        fn export_tab_sits_at_the_panel_foot_and_auto_tone_under_tone() {
            let mut app = loupe("export");
            let t = crate::i18n::t();
            let painted = settled(&mut app);
            assert!(painted.has(t.auto_tone), "{:?}", painted.texts());
            let tone = painted.pos_of(t.section(crate::develop::Section::Tone));
            let auto = painted.pos_of(t.auto_tone);
            assert!(
                (auto.y - tone.y).abs() < 4.0,
                "Auto Tone shares the Tone header's row: {tone:?} vs {auto:?}"
            );

            for tab in [DevelopTab::Sliders, DevelopTab::Masks] {
                app.set_develop_tab(tab);
                let painted = settled(&mut app);
                let export = painted.pos_of(t.export_jpg);
                assert!(
                    export.y > painted.pos_of(t.tab_masks).y + 100.0,
                    "the Export tab sits below the panel's content"
                );
                let (actions, _) = click(&mut app, export);
                assert!(
                    actions.iter().any(|a| matches!(a, UiAction::ToggleExportForm)),
                    "Export on {tab:?} opens the Export form"
                );
            }

            app.toggle_export_form();
            let painted = settled(&mut app);
            assert!(painted.has(t.export_run), "{:?}", painted.texts());
            let (actions, _) = click(&mut app, painted.pos_of(t.develop));
            assert!(
                actions.iter().any(|a| matches!(a, UiAction::ToggleExportForm)),
                "Develop on the Export form goes back to the sliders"
            );
        }

        #[test]
        fn leaving_masks_disarms_touch_up() {
            let mut app = loupe("disarm");
            app.set_develop_tab(DevelopTab::Masks);
            app.tool = LoupeTool::TouchUp;
            app.set_develop_tab(DevelopTab::Sliders);
            assert!(!app.touchup_active());
        }

        #[test]
        fn the_wheel_over_the_image_grows_the_brush() {
            let mut app = loupe("wheel");
            app.set_develop_tab(DevelopTab::Masks);
            app.tool = LoupeTool::TouchUp;
            let _ = settled(&mut app);
            let before = app.touchup_radius();

            let over_image = egui::pos2(480.0, 300.0);
            let wheel = egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 40.0),
                phase: egui::TouchPhase::Move,
                modifiers: Default::default(),
            };
            let _ = frame(&mut app, vec![egui::Event::PointerMoved(over_image)]);
            let mut grew = None;
            // egui spreads a wheel step over a few frames.
            for events in [vec![wheel], Vec::new(), Vec::new(), Vec::new()] {
                let (actions, _) = frame(&mut app, events);
                grew = grew.or(actions.into_iter().find_map(|a| match a {
                    UiAction::SetTouchUpRadius(r) => Some(r),
                    _ => None,
                }));
            }
            let grew = grew.expect("the wheel resizes the brush");
            assert!(grew > before, "{grew} > {before}");
        }

        #[test]
        fn shift_wheel_over_the_image_changes_the_feather_not_the_size() {
            let mut app = loupe("shift-wheel");
            app.set_develop_tab(DevelopTab::Masks);
            app.tool = LoupeTool::TouchUp;
            app.set_touchup_feather(0.5);
            let _ = settled(&mut app);

            let over_image = egui::pos2(480.0, 300.0);
            let _ = frame(&mut app, vec![egui::Event::PointerMoved(over_image)]);
            let mut feathers = |dy: f32| {
                let wheel = egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, dy),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::SHIFT,
                };
                let mut feather = None;
                for events in [vec![wheel], Vec::new(), Vec::new(), Vec::new()] {
                    let (actions, _) =
                        frame_with_modifiers(&mut app, events, egui::Modifiers::SHIFT);
                    for a in actions {
                        match a {
                            UiAction::SetTouchUpFeather(f) => feather = Some(f),
                            UiAction::SetTouchUpRadius(_) => {
                                panic!("Shift+wheel resized the brush")
                            }
                            _ => {}
                        }
                    }
                }
                feather.expect("Shift+wheel changes the feather")
            };
            assert!(feathers(40.0) > 0.5, "wheel up softens the brush");
            assert!(feathers(-40.0) < 0.5, "wheel down hardens the brush");
        }

        fn spot(u: f32) -> TouchUp {
            TouchUp {
                center: [u, 0.5],
                radius: 0.02,
                source: [u, 0.3],
                feather: TOUCHUP_FEATHER,
                delta: [0.0; 3],
            }
        }

        fn centers(app: &App) -> Vec<f32> {
            app.current_touchups().iter().map(|s| s.center[0]).collect()
        }

        /// Clicks a Masks-tab button and applies whatever it emitted. Returns
        /// whether it emitted anything, which a disabled button does not.
        fn press(app: &mut App, label: &str) -> bool {
            let painted = settled(app);
            let (actions, _) = click(app, painted.pos_of(label));
            let emitted = !actions.is_empty();
            app.apply_ui_actions(actions);
            emitted
        }

        #[test]
        fn undo_reverses_adds_and_deletes() {
            let mut app = loupe("undo");
            app.set_develop_tab(DevelopTab::Masks);
            app.tool = LoupeTool::TouchUp;
            let t = crate::i18n::t();
            for u in [0.2, 0.4, 0.6] {
                let mut all = app.current_touchups().to_vec();
                all.push(spot(u));
                assert!(app.edit_touchups(all));
            }

            let painted = settled(&mut app);
            assert!(
                !painted.texts().contains(&"Undo"),
                "Undo is Cmd+Z only, with no button"
            );

            app.touchup_selected = Some(0);
            press(&mut app, t.delete);
            assert_eq!(centers(&app), [0.4, 0.6]);

            app.undo_touchup();
            assert_eq!(
                centers(&app),
                [0.2, 0.4, 0.6],
                "Undo brings the deleted spot back"
            );
            app.undo_touchup();
            assert_eq!(
                centers(&app),
                [0.2, 0.4],
                "then steps back through the adds"
            );
            app.undo_touchup();
            app.undo_touchup();
            assert!(centers(&app).is_empty());
            app.undo_touchup();
            assert!(
                centers(&app).is_empty(),
                "Undo with no history does nothing"
            );
        }

        #[test]
        fn the_switch_arms_touch_up_and_unlocks_the_brush_size() {
            let mut app = loupe("switch");
            app.set_develop_tab(DevelopTab::Masks);
            let t = crate::i18n::t();
            let painted = settled(&mut app);
            // The switch sits just left of Delete; the Size slider right of its
            // label, on the row below.
            let switch = painted.pos_of(t.delete) - egui::vec2(20.0, 0.0);
            let slider = painted.pos_of(t.brush_size) + egui::vec2(100.0, 0.0);
            let resizes = |actions: &[UiAction]| {
                actions
                    .iter()
                    .any(|a| matches!(a, UiAction::SetTouchUpRadius(_)))
            };

            let (actions, _) = click(&mut app, slider);
            assert!(
                !resizes(&actions),
                "the brush size is locked while Touch Up is off"
            );

            let (actions, _) = click(&mut app, switch);
            assert!(actions.iter().any(|a| matches!(a, UiAction::ToggleTouchUp)));
            app.apply_ui_actions(actions);
            assert!(app.touchup_active());

            let _ = settled(&mut app);
            let (actions, _) = click(&mut app, slider);
            assert!(
                resizes(&actions),
                "the brush size is live once Touch Up is on"
            );
        }

        #[test]
        fn the_feather_slider_is_live_only_while_touch_up_is_armed() {
            let mut app = loupe("feather-slider");
            app.set_develop_tab(DevelopTab::Masks);
            let t = crate::i18n::t();
            let painted = settled(&mut app);
            let feather = painted.pos_of(t.feather);
            assert!(
                feather.y > painted.pos_of(t.brush_size).y,
                "Feather sits on its own row below Size"
            );
            let slider = feather + egui::vec2(100.0, 0.0);
            let feathered = |actions: &[UiAction]| {
                actions.iter().find_map(|a| match a {
                    UiAction::SetTouchUpFeather(f) => Some(*f),
                    _ => None,
                })
            };

            let (actions, _) = click(&mut app, slider);
            assert_eq!(feathered(&actions), None, "locked while Touch Up is off");

            app.toggle_touchup();
            let _ = settled(&mut app);
            let (actions, _) = click(&mut app, slider);
            let f = feathered(&actions).expect("live once Touch Up is on");
            assert!(
                f < 1.0,
                "a click mid-track lowers the feather from 1.0: {f}"
            );
            app.apply_ui_actions(actions);
            assert_eq!(app.touchup_feather(), f);
        }

        #[test]
        fn a_new_spot_takes_the_brush_feather() {
            let mut app = loupe("feather-spot");
            let path = app.shown.path().unwrap().to_path_buf();
            let (w, h) = (400, 300);
            app.shown = Shown::Preview(path.clone(), w, h);
            app.source_size = Some((w, h));
            let mut loader = crate::loader::Loader::new(16384, crate::cache_limits::CacheLimits::PLATFORM);
            loader.insert_full_external(
                path,
                std::sync::Arc::new(crate::image_decode::DecodedImage::new_tracked(
                    crate::image_decode::DecodedImageFields {
                        width: w,
                        height: h,
                        rgba: vec![128; (w * h * 4) as usize],
                        pixel_format: Default::default(),
                    },
                )),
            );
            app.loader = Some(loader);

            app.set_touchup_feather(0.3);
            app.add_touchup(0.5, 0.5);
            let spots = app.current_touchups();
            assert_eq!(spots.len(), 1);
            assert_eq!(spots[0].feather, 0.3);
        }

        #[test]
        fn delete_is_disabled_until_a_spot_is_selected() {
            let mut app = loupe("delete");
            app.set_develop_tab(DevelopTab::Masks);
            app.tool = LoupeTool::TouchUp;
            let t = crate::i18n::t();
            assert!(app.edit_touchups(vec![spot(0.2), spot(0.4)]));
            assert_eq!(app.touchup_selected(), None);
            assert!(!press(&mut app, t.delete));
            assert_eq!(centers(&app), [0.2, 0.4]);

            app.touchup_selected = Some(1);
            press(&mut app, t.delete);
            assert_eq!(centers(&app), [0.2]);
        }
    }
}
