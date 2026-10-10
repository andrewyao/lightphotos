// SPDX-License-Identifier: MIT OR Apache-2.0

//! Headless script driver for live checks. `--drive <script>` runs the real
//! `App`, egui frame and renderer against a hidden window, so a check never
//! touches the desktop and several can run at once. Settings go to a scratch
//! directory for the run, never the person's own.
//!
//! A script is one step per line. `#` starts a comment.
//!
//! ```text
//! size 1200 800            window size in points; the first one sets the window up
//! key cmd+shift+u          a chord: cmd, ctrl, shift, alt, then a key name
//! type Holiday             text for a focused field
//! click 40 60 [shift]      pointer at (x, y) points, optional modifiers
//! dblclick 40 60
//! click-cell 2 [shift]     the Grid or filmstrip cell at that position, from the last frame's layout
//! dblclick-cell 2
//! rclick 40 60             a right click, which opens a photo's menu
//! rclick-cell 2
//! click-badge 2            the stack badge on the cell at that position, which expands or collapses the stack
//! move 40 60               pointer to (x, y) points, no click
//! scroll 0 5 [shift]       wheel lines; up is positive, so this zooms the Loupe in
//! drag 100 100 300 200
//! idle                     wait until decodes, sidecar writes, batches and animations settle
//! shot grid                write <out dir>/grid.png
//! state                    print one JSON line of the app's state
//! quit
//! ```
//!
//! With no photo or folder after the flags, the run starts on the home page.
//! Settings start empty, so that is a first launch.
//!
//! Keys cannot take the `WindowEvent::KeyboardInput` road: winit's `KeyEvent`
//! has a private field, so nothing outside winit can build one. The driver
//! instead pushes the egui events `egui_winit` would have and calls
//! `App::key_input`, the half of `main.rs`'s handler that runs after egui.
//! Pointer events are built whole and go through `App::window_event`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::{Window, WindowId};

use crate::app::{App, Region, ViewMode};
use crate::renderer::{Renderer, RgbFrame};
use crate::shell::macos_delegate::UserEvent;

const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SIZE: (u32, u32) = (1200, 800);
/// The gap between two click steps. egui counts a click within 0.3 s of the
/// last one as a double click and within 0.6 s of the one before as a
/// triple, judged by distance from the last click only, so a script's
/// back-to-back clicks on different cells would read as a triple click on
/// the second. A person cannot click that fast; the driver waits it out.
const CLICK_GAP: Duration = Duration::from_millis(650);

#[derive(Debug)]
pub(crate) struct Args {
    script: PathBuf,
    steps: Vec<Step>,
    out_dir: PathBuf,
    /// The photo or folder to open. Without one the run starts on the home
    /// page, as a launch with no path does.
    target: Option<PathBuf>,
}

impl Args {
    pub(crate) fn from_env() -> Result<Option<Args>, String> {
        Args::parse(std::env::args().skip(1))
    }

    fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Args>, String> {
        let args: Vec<String> = args.into_iter().collect();
        if !args.iter().any(|a| a == "--drive" || a == "--drive-out") {
            return Ok(None);
        }
        let mut script = None;
        let mut out_dir = None;
        let mut target = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--drive" => {
                    script = Some(args.next().ok_or("--drive needs a script path")?);
                }
                "--drive-out" => {
                    out_dir = Some(args.next().ok_or("--drive-out needs a directory")?);
                }
                _ if target.is_none() => target = Some(arg),
                _ => return Err(format!("unexpected argument: {arg}")),
            }
        }
        let script = PathBuf::from(script.ok_or("--drive-out needs --drive")?);
        let target = target.map(PathBuf::from);
        if let Some(target) = target.as_ref().filter(|t| !t.exists()) {
            return Err(format!("no such path: {}", target.display()));
        }
        let text =
            std::fs::read_to_string(&script).map_err(|e| format!("{}: {e}", script.display()))?;
        let steps = parse_script(&text).map_err(|e| format!("{}: {e}", script.display()))?;
        let out_dir = PathBuf::from(out_dir.unwrap_or_else(|| ".".into()));
        std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
        Ok(Some(Args {
            script,
            steps,
            out_dir,
            target,
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ClickKind {
    Single,
    Double,
    Right,
}

impl ClickKind {
    fn of(cmd: &str) -> ClickKind {
        if cmd.starts_with("dbl") {
            ClickKind::Double
        } else if cmd.starts_with('r') {
            ClickKind::Right
        } else {
            ClickKind::Single
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    Size(u32, u32),
    Key(Chord),
    Type(String),
    Click {
        at: Target,
        kind: ClickKind,
        mods: ModifiersState,
    },
    Move(f32, f32),
    Scroll {
        dx: f32,
        dy: f32,
        mods: ModifiersState,
    },
    Drag {
        from: (f32, f32),
        to: (f32, f32),
    },
    Idle,
    Shot(String),
    State,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Target {
    Point(f32, f32),
    Cell(usize),
    Badge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chord {
    mods: ModifiersState,
    name: &'static str,
    code: KeyCode,
    key: egui::Key,
}

const KEYS: &[(&str, KeyCode, egui::Key)] = &[
    ("a", KeyCode::KeyA, egui::Key::A),
    ("b", KeyCode::KeyB, egui::Key::B),
    ("c", KeyCode::KeyC, egui::Key::C),
    ("d", KeyCode::KeyD, egui::Key::D),
    ("e", KeyCode::KeyE, egui::Key::E),
    ("f", KeyCode::KeyF, egui::Key::F),
    ("g", KeyCode::KeyG, egui::Key::G),
    ("h", KeyCode::KeyH, egui::Key::H),
    ("i", KeyCode::KeyI, egui::Key::I),
    ("j", KeyCode::KeyJ, egui::Key::J),
    ("k", KeyCode::KeyK, egui::Key::K),
    ("l", KeyCode::KeyL, egui::Key::L),
    ("m", KeyCode::KeyM, egui::Key::M),
    ("n", KeyCode::KeyN, egui::Key::N),
    ("o", KeyCode::KeyO, egui::Key::O),
    ("p", KeyCode::KeyP, egui::Key::P),
    ("q", KeyCode::KeyQ, egui::Key::Q),
    ("r", KeyCode::KeyR, egui::Key::R),
    ("s", KeyCode::KeyS, egui::Key::S),
    ("t", KeyCode::KeyT, egui::Key::T),
    ("u", KeyCode::KeyU, egui::Key::U),
    ("v", KeyCode::KeyV, egui::Key::V),
    ("w", KeyCode::KeyW, egui::Key::W),
    ("x", KeyCode::KeyX, egui::Key::X),
    ("y", KeyCode::KeyY, egui::Key::Y),
    ("z", KeyCode::KeyZ, egui::Key::Z),
    ("0", KeyCode::Digit0, egui::Key::Num0),
    ("1", KeyCode::Digit1, egui::Key::Num1),
    ("2", KeyCode::Digit2, egui::Key::Num2),
    ("3", KeyCode::Digit3, egui::Key::Num3),
    ("4", KeyCode::Digit4, egui::Key::Num4),
    ("5", KeyCode::Digit5, egui::Key::Num5),
    ("6", KeyCode::Digit6, egui::Key::Num6),
    ("7", KeyCode::Digit7, egui::Key::Num7),
    ("8", KeyCode::Digit8, egui::Key::Num8),
    ("9", KeyCode::Digit9, egui::Key::Num9),
    ("[", KeyCode::BracketLeft, egui::Key::OpenBracket),
    ("]", KeyCode::BracketRight, egui::Key::CloseBracket),
    ("=", KeyCode::Equal, egui::Key::Equals),
    ("-", KeyCode::Minus, egui::Key::Minus),
    (",", KeyCode::Comma, egui::Key::Comma),
    (".", KeyCode::Period, egui::Key::Period),
    ("/", KeyCode::Slash, egui::Key::Slash),
    ("left", KeyCode::ArrowLeft, egui::Key::ArrowLeft),
    ("right", KeyCode::ArrowRight, egui::Key::ArrowRight),
    ("up", KeyCode::ArrowUp, egui::Key::ArrowUp),
    ("down", KeyCode::ArrowDown, egui::Key::ArrowDown),
    ("enter", KeyCode::Enter, egui::Key::Enter),
    ("return", KeyCode::Enter, egui::Key::Enter),
    ("escape", KeyCode::Escape, egui::Key::Escape),
    ("esc", KeyCode::Escape, egui::Key::Escape),
    ("space", KeyCode::Space, egui::Key::Space),
    ("tab", KeyCode::Tab, egui::Key::Tab),
    ("delete", KeyCode::Delete, egui::Key::Delete),
    ("backspace", KeyCode::Backspace, egui::Key::Backspace),
    ("pageup", KeyCode::PageUp, egui::Key::PageUp),
    ("pagedown", KeyCode::PageDown, egui::Key::PageDown),
    ("f6", KeyCode::F6, egui::Key::F6),
];

fn modifier(name: &str) -> Option<ModifiersState> {
    Some(match name {
        "cmd" | "super" | "meta" => ModifiersState::SUPER,
        "ctrl" | "control" => ModifiersState::CONTROL,
        "shift" => ModifiersState::SHIFT,
        "alt" | "opt" | "option" => ModifiersState::ALT,
        _ => return None,
    })
}

fn parse_chord(s: &str) -> Result<Chord, String> {
    let lower = s.to_ascii_lowercase();
    let mut parts = lower.split('+').peekable();
    let mut mods = ModifiersState::empty();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let (name, code, key) = KEYS
                .iter()
                .find(|(name, _, _)| *name == part)
                .ok_or_else(|| format!("unknown key `{part}`"))?;
            return Ok(Chord {
                mods,
                name,
                code: *code,
                key: *key,
            });
        }
        mods |= modifier(part).ok_or_else(|| format!("unknown modifier `{part}`"))?;
    }
    Err("empty chord".into())
}

fn parse_mods(words: &[&str]) -> Result<ModifiersState, String> {
    let mut mods = ModifiersState::empty();
    for word in words.iter().flat_map(|w| w.split('+')) {
        mods |= modifier(&word.to_ascii_lowercase())
            .ok_or_else(|| format!("unknown modifier `{word}`"))?;
    }
    Ok(mods)
}

fn number<T: std::str::FromStr>(word: Option<&str>, what: &str) -> Result<T, String> {
    word.ok_or_else(|| format!("missing {what}"))?
        .parse()
        .map_err(|_| format!("bad {what} `{}`", word.unwrap_or_default()))
}

/// A point or wheel amount. `f32` parses `inf` and `NaN`, which would leave
/// the app's pointer and zoom state non-finite.
fn coord(word: Option<&str>, what: &str) -> Result<f32, String> {
    let v: f32 = number(word, what)?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(format!("bad {what} `{v}`"))
    }
}

fn parse_step(line: &str) -> Result<Step, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let (cmd, args) = words.split_first().ok_or("empty step")?;
    let step = match *cmd {
        "size" => {
            let w = number(args.first().copied(), "width")?;
            let h = number(args.get(1).copied(), "height")?;
            if w == 0 || h == 0 || args.len() != 2 {
                return Err("size takes a width and a height above zero".into());
            }
            Step::Size(w, h)
        }
        "key" => {
            let [chord] = args else {
                return Err("key takes one chord".into());
            };
            Step::Key(parse_chord(chord)?)
        }
        "type" => {
            let rest = line.trim_start().strip_prefix("type").unwrap_or_default();
            let text = rest.trim();
            if text.is_empty() {
                return Err("type takes text".into());
            }
            Step::Type(text.to_string())
        }
        "click" | "dblclick" | "rclick" => {
            let x = coord(args.first().copied(), "x")?;
            let y = coord(args.get(1).copied(), "y")?;
            Step::Click {
                at: Target::Point(x, y),
                kind: ClickKind::of(cmd),
                mods: parse_mods(args.get(2..).unwrap_or_default())?,
            }
        }
        "click-cell" | "dblclick-cell" | "rclick-cell" => Step::Click {
            at: Target::Cell(number(args.first().copied(), "cell position")?),
            kind: ClickKind::of(cmd),
            mods: parse_mods(args.get(1..).unwrap_or_default())?,
        },
        "click-badge" => {
            let [pos] = args else {
                return Err("click-badge takes a cell position".into());
            };
            Step::Click {
                at: Target::Badge(number(Some(*pos), "cell position")?),
                kind: ClickKind::Single,
                mods: ModifiersState::empty(),
            }
        }
        "move" => Step::Move(
            coord(args.first().copied(), "x")?,
            coord(args.get(1).copied(), "y")?,
        ),
        "scroll" => Step::Scroll {
            dx: coord(args.first().copied(), "dx")?,
            dy: coord(args.get(1).copied(), "dy")?,
            mods: parse_mods(args.get(2..).unwrap_or_default())?,
        },
        "drag" => {
            if args.len() != 4 {
                return Err("drag takes x0 y0 x1 y1".into());
            }
            Step::Drag {
                from: (coord(Some(args[0]), "x0")?, coord(Some(args[1]), "y0")?),
                to: (coord(Some(args[2]), "x1")?, coord(Some(args[3]), "y1")?),
            }
        }
        "idle" => Step::Idle,
        "shot" => {
            let [slug] = args else {
                return Err("shot takes one name".into());
            };
            if !slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "shot name `{slug}` may only use letters, digits, - and _"
                ));
            }
            Step::Shot(slug.to_string())
        }
        "state" => Step::State,
        "quit" => Step::Quit,
        _ => return Err(format!("unknown step `{cmd}`")),
    };
    let arity_ok = match step {
        Step::Idle | Step::State | Step::Quit => args.is_empty(),
        _ => true,
    };
    if !arity_ok {
        return Err(format!("{cmd} takes no arguments"));
    }
    Ok(step)
}

fn parse_script(text: &str) -> Result<Vec<Step>, ParseError> {
    let mut steps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let step = parse_step(line).map_err(|message| ParseError {
            line: i + 1,
            message,
        })?;
        steps.push(step);
    }
    Ok(steps)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub(crate) line: usize,
    pub(crate) message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

pub(crate) fn run(args: Args) -> i32 {
    let config_dir = std::env::temp_dir().join(format!("lightphotos-drive-{}", std::process::id()));
    crate::persist::prefs::override_config_dir(config_dir.clone());
    crate::shell::dialog::cancel_all_pickers();
    crate::i18n::init();

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder
            .with_activation_policy(ActivationPolicy::Prohibited)
            .with_activate_ignoring_other_apps(false)
            .with_default_menu(false);
    }
    let event_loop = match builder.build() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[drive] failed: build event loop: {e}");
            return 1;
        }
    };

    let size = args
        .steps
        .iter()
        .find_map(|s| match s {
            Step::Size(w, h) => Some((*w, *h)),
            _ => None,
        })
        .unwrap_or(DEFAULT_SIZE);
    eprintln!(
        "[drive] {} ({} steps) on {}, shots to {}",
        args.script.display(),
        args.steps.len(),
        args.target
            .as_deref()
            .map_or("the home page".into(), |t| t.display().to_string()),
        args.out_dir.display()
    );
    let mut driver = Driver {
        app: App::new(args.target),
        steps: args.steps,
        out_dir: args.out_dir,
        size,
        last_click: None,
        outcome: Ok(()),
    };
    let ran = event_loop.run_app(&mut driver);
    let _ = std::fs::remove_dir_all(&config_dir);
    match ran.map_err(|e| e.to_string()).and(driver.outcome) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[drive] failed: {e}");
            1
        }
    }
}

struct Driver {
    app: App,
    steps: Vec<Step>,
    out_dir: PathBuf,
    size: (u32, u32),
    last_click: Option<Instant>,
    outcome: Result<(), String>,
}

impl ApplicationHandler<UserEvent> for Driver {
    /// The whole script runs here. `ActiveEventLoop` exists only inside a
    /// callback, and the loader, catalog queue and export pool are polled,
    /// not event-driven, so nothing needs the loop to turn.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("LightPhotos")
            .with_visible(false)
            .with_inner_size(LogicalSize::new(self.size.0, self.size.1));
        let started = event_loop
            .create_window(attrs)
            .map_err(|e| format!("create window: {e}"))
            .map(Arc::new)
            .and_then(|window| {
                let size = window.inner_size();
                let renderer = pollster::block_on(Renderer::new(window.clone(), size))?;
                Ok((window, size, renderer))
            });
        let (window, size, mut renderer) = match started {
            Ok(started) => started,
            Err(e) => {
                self.outcome = Err(e);
                event_loop.exit();
                return;
            }
        };
        renderer.render_offscreen();
        crate::finish_window_setup(&mut self.app, window, renderer, size);
        // A blinking caret would ask for a repaint forever and make two
        // shots of one state differ.
        self.app
            .egui_ctx
            .global_style_mut(|s| s.visuals.text_cursor.blink = false);

        let steps = std::mem::take(&mut self.steps);
        self.outcome = self.run_steps(event_loop, &steps);
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        ApplicationHandler::exiting(&mut self.app, event_loop);
    }
}

impl Driver {
    fn run_steps(&mut self, el: &ActiveEventLoop, steps: &[Step]) -> Result<(), String> {
        self.idle()?;
        for (i, step) in steps.iter().enumerate() {
            eprintln!("[drive] step {}: {step:?}", i + 1);
            match step {
                Step::Size(w, h) => self.resize(el, *w, *h),
                Step::Key(chord) => self.key(el, *chord),
                Step::Type(text) => self.push_egui(egui::Event::Text(text.clone())),
                Step::Click { at, kind, mods } => {
                    let (x, y) = self.resolve(*at)?;
                    self.click(el, x, y, *kind, *mods);
                }
                Step::Move(x, y) => self.move_to(el, *x, *y),
                Step::Scroll { dx, dy, mods } => {
                    self.window_event(el, WindowEvent::ModifiersChanged((*mods).into()));
                    self.window_event(
                        el,
                        WindowEvent::MouseWheel {
                            device_id: DeviceId::dummy(),
                            delta: MouseScrollDelta::LineDelta(*dx, *dy),
                            phase: TouchPhase::Moved,
                        },
                    );
                    self.window_event(
                        el,
                        WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
                    );
                }
                Step::Drag { from, to } => self.drag(el, *from, *to),
                Step::Idle => self.idle()?,
                Step::Shot(slug) => self.shot(slug)?,
                Step::State => println!("{}", self.state_json()?),
                Step::Quit => return Ok(()),
            }
            self.frame();
        }
        Ok(())
    }

    fn frame(&mut self) {
        self.app.pump();
        self.app.redraw();
    }

    fn idle(&mut self) -> Result<(), String> {
        let start = Instant::now();
        loop {
            if self.settled_across_a_frame() {
                self.app.signals.flush_blocking(IDLE_TIMEOUT);
                return Ok(());
            }
            if start.elapsed() > IDLE_TIMEOUT {
                return Err(format!("idle: still busy after {IDLE_TIMEOUT:?}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn settled_across_a_frame(&mut self) -> bool {
        let quiet_before = self.app.pump().is_none();
        self.app.redraw();
        let quiet_after = self.app.pump().is_none();
        quiet_before && quiet_after && !self.app.batch_running() && self.app.repaint_at.is_none()
    }

    fn scale(&self) -> f64 {
        self.app.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }

    fn window_event(&mut self, el: &ActiveEventLoop, event: WindowEvent) {
        let Some(id) = self.app.window.as_ref().map(|w| w.id()) else {
            return;
        };
        ApplicationHandler::window_event(&mut self.app, el, id, event);
    }

    fn push_egui(&mut self, event: egui::Event) {
        if let Some(state) = self.app.egui_state.as_mut() {
            state.egui_input_mut().events.push(event);
        }
    }

    fn resize(&mut self, el: &ActiveEventLoop, w: u32, h: u32) {
        let Some(window) = self.app.window.clone() else {
            return;
        };
        // A hidden macOS window applies the size without returning it, and no
        // Resized event reaches a script, so forward the size it now has.
        let size = window
            .request_inner_size(LogicalSize::new(w, h))
            .unwrap_or_else(|| window.inner_size());
        self.window_event(el, WindowEvent::Resized(size));
    }

    /// Mirrors `egui_winit::State::on_keyboard_input`: a `Key` event, a
    /// `Text` event for a printable key without Cmd or Ctrl, and egui's
    /// consumed verdict, then the app's own handler.
    fn key(&mut self, el: &ActiveEventLoop, chord: Chord) {
        self.window_event(el, WindowEvent::ModifiersChanged(chord.mods.into()));
        let text = (chord.name.chars().count() == 1
            && !(chord.mods.super_key() || chord.mods.control_key()))
        .then(|| {
            if chord.mods.shift_key() {
                chord.name.to_ascii_uppercase()
            } else {
                chord.name.to_string()
            }
        });
        for pressed in [true, false] {
            let consumed =
                self.app.egui_ctx.egui_wants_keyboard_input() || chord.key == egui::Key::Tab;
            let modifiers = self
                .app
                .egui_state
                .as_ref()
                .map(|s| s.egui_input().modifiers)
                .unwrap_or_default();
            self.push_egui(egui::Event::Key {
                key: chord.key,
                physical_key: Some(chord.key),
                pressed,
                repeat: false,
                modifiers,
            });
            if pressed {
                if let Some(text) = &text {
                    self.push_egui(egui::Event::Text(text.clone()));
                }
            }
            let state = if pressed {
                ElementState::Pressed
            } else {
                ElementState::Released
            };
            self.app.key_input(chord.code, state, consumed);
            self.frame();
        }
        self.window_event(
            el,
            WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
        );
    }

    fn resolve(&self, at: Target) -> Result<(f32, f32), String> {
        match at {
            Target::Point(x, y) => Ok((x, y)),
            Target::Cell(pos) => {
                let rect = self.app.cell_rect(pos).ok_or_else(|| {
                    let (start, end) = match self.app.mode() {
                        ViewMode::Grid => self.app.grid_range(),
                        _ => self.app.strip_range(),
                    };
                    format!("cell {pos} is not on screen (cells {start}..{end} are)")
                })?;
                Ok((rect.center().x, rect.center().y))
            }
            Target::Badge(pos) => {
                let rect = self
                    .app
                    .badge_rect(pos)
                    .ok_or_else(|| format!("cell {pos} drew no stack badge"))?;
                Ok((rect.center().x, rect.center().y))
            }
        }
    }

    fn move_to(&mut self, el: &ActiveEventLoop, x: f32, y: f32) {
        let scale = self.scale();
        self.window_event(
            el,
            WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: PhysicalPosition::new(x as f64 * scale, y as f64 * scale),
            },
        );
    }

    fn button(&mut self, el: &ActiveEventLoop, state: ElementState) {
        self.press(el, MouseButton::Left, state);
    }

    fn press(&mut self, el: &ActiveEventLoop, button: MouseButton, state: ElementState) {
        self.window_event(
            el,
            WindowEvent::MouseInput {
                device_id: DeviceId::dummy(),
                state,
                button,
            },
        );
        self.frame();
    }

    fn click(
        &mut self,
        el: &ActiveEventLoop,
        x: f32,
        y: f32,
        kind: ClickKind,
        mods: ModifiersState,
    ) {
        if let Some(last) = self.last_click {
            while last.elapsed() < CLICK_GAP {
                self.frame();
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.window_event(el, WindowEvent::ModifiersChanged(mods.into()));
        self.move_to(el, x, y);
        self.frame();
        let (button, times) = match kind {
            ClickKind::Single => (MouseButton::Left, 1),
            ClickKind::Double => (MouseButton::Left, 2),
            ClickKind::Right => (MouseButton::Right, 1),
        };
        for _ in 0..times {
            self.press(el, button, ElementState::Pressed);
            self.press(el, button, ElementState::Released);
        }
        self.window_event(
            el,
            WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
        );
        self.last_click = Some(Instant::now());
    }

    fn drag(&mut self, el: &ActiveEventLoop, from: (f32, f32), to: (f32, f32)) {
        self.move_to(el, from.0, from.1);
        self.frame();
        self.button(el, ElementState::Pressed);
        self.move_to(el, (from.0 + to.0) / 2.0, (from.1 + to.1) / 2.0);
        self.frame();
        self.move_to(el, to.0, to.1);
        self.frame();
        self.button(el, ElementState::Released);
        self.last_click = Some(Instant::now());
    }

    fn shot(&mut self, slug: &str) -> Result<(), String> {
        let frame = self
            .app
            .renderer
            .as_ref()
            .and_then(|r| r.read_offscreen_blocking())
            .ok_or("shot: no offscreen frame to read")?;
        let path = self.out_dir.join(format!("{slug}.png"));
        write_png(&path, &frame)?;
        eprintln!(
            "[drive] wrote {} ({}x{})",
            path.display(),
            frame.width,
            frame.height
        );
        Ok(())
    }

    fn state_json(&self) -> Result<String, String> {
        serde_json::to_string(&State::of(&self.app)).map_err(|e| e.to_string())
    }
}

fn write_png(path: &Path, frame: &RgbFrame) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = std::fs::File::create(path).map_err(|e| err(&e))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| err(&e))?;
    writer.write_image_data(&frame.rgb).map_err(|e| err(&e))?;
    writer.finish().map_err(|e| err(&e))
}

#[derive(serde::Serialize)]
struct State {
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
    /// The keyboard-focused region.
    focus: Region,
    /// Loupe zoom relative to fit-to-window; absent in the Grid.
    zoom: Option<f32>,
    /// The status toast, while it shows.
    status: Option<String>,
    /// Visible thumbnails whose decode failed for good.
    failed_thumbs: usize,
    /// The Folders tree's roots.
    folder_roots: Vec<std::path::PathBuf>,
    /// The folder the Grid shows.
    folder: Option<std::path::PathBuf>,
}

impl State {
    fn of(app: &App) -> State {
        let mode = app.mode();
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
            folder_roots: app.folder_roots().to_vec(),
            folder: app.folder_sel(),
            sel: app.sel(),
            selected: app.selected_positions(),
            photo: app
                .selected_path()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
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

    fn chord(s: &str) -> Chord {
        parse_chord(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn every_step_form_parses() {
        let script = "\
# a comment line
size 1200 800
key cmd+shift+u
type Holiday snaps  # trailing comment
click 40 60
dblclick 40.5 60 shift
click-cell 2 shift
dblclick-cell 3
rclick 10 20
rclick-cell 4
click-badge 5
scroll 0 -5 shift
drag 100 100 300 200
idle
shot grid-1
state
quit
";
        let shift = ModifiersState::SHIFT;
        assert_eq!(
            parse_script(script).unwrap(),
            vec![
                Step::Size(1200, 800),
                Step::Key(chord("cmd+shift+u")),
                Step::Type("Holiday snaps".into()),
                Step::Click {
                    at: Target::Point(40.0, 60.0),
                    kind: ClickKind::Single,
                    mods: ModifiersState::empty(),
                },
                Step::Click {
                    at: Target::Point(40.5, 60.0),
                    kind: ClickKind::Double,
                    mods: shift,
                },
                Step::Click {
                    at: Target::Cell(2),
                    kind: ClickKind::Single,
                    mods: shift,
                },
                Step::Click {
                    at: Target::Cell(3),
                    kind: ClickKind::Double,
                    mods: ModifiersState::empty(),
                },
                Step::Click {
                    at: Target::Point(10.0, 20.0),
                    kind: ClickKind::Right,
                    mods: ModifiersState::empty(),
                },
                Step::Click {
                    at: Target::Cell(4),
                    kind: ClickKind::Right,
                    mods: ModifiersState::empty(),
                },
                Step::Click {
                    at: Target::Badge(5),
                    kind: ClickKind::Single,
                    mods: ModifiersState::empty(),
                },
                Step::Scroll {
                    dx: 0.0,
                    dy: -5.0,
                    mods: shift,
                },
                Step::Drag {
                    from: (100.0, 100.0),
                    to: (300.0, 200.0),
                },
                Step::Idle,
                Step::Shot("grid-1".into()),
                Step::State,
                Step::Quit,
            ]
        );
    }

    #[test]
    fn a_bad_line_reports_its_number() {
        let err = parse_script("idle\n\nclik 1 2\nstate\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert_eq!(err.message, "unknown step `clik`");
        assert_eq!(err.to_string(), "line 3: unknown step `clik`");

        for (script, line) in [
            ("key cmd+bogus", 1),
            ("idle\nsize 0 5", 2),
            ("shot grid.png", 1),
            ("click 1", 1),
            ("idle now", 1),
            ("drag 1 2 3", 1),
            ("click NaN 5", 1),
            ("move 5 inf", 1),
            ("scroll 0 1e39", 1),
            ("drag 0 0 -inf 1", 1),
        ] {
            let err = parse_script(script).unwrap_err();
            assert_eq!(err.line, line, "{script:?}: {}", err.message);
        }
    }

    #[test]
    fn chords_map_to_the_codes_keys_rs_matches() {
        let none = ModifiersState::empty();
        let cmd = ModifiersState::SUPER;
        for (name, mods, code) in [
            ("4", none, KeyCode::Digit4),
            ("g", none, KeyCode::KeyG),
            ("delete", none, KeyCode::Delete),
            ("return", none, KeyCode::Enter),
            ("enter", none, KeyCode::Enter),
            ("escape", none, KeyCode::Escape),
            ("space", none, KeyCode::Space),
            ("[", none, KeyCode::BracketLeft),
            ("cmd+a", cmd, KeyCode::KeyA),
            ("ctrl+a", ModifiersState::CONTROL, KeyCode::KeyA),
            ("shift+right", ModifiersState::SHIFT, KeyCode::ArrowRight),
            ("cmd+shift+u", cmd | ModifiersState::SHIFT, KeyCode::KeyU),
            ("alt+=", ModifiersState::ALT, KeyCode::Equal),
            ("Cmd+G", cmd, KeyCode::KeyG),
        ] {
            let c = chord(name);
            assert_eq!((c.mods, c.code), (mods, code), "{name}");
        }
    }

    #[test]
    fn every_key_name_parses_to_its_own_row() {
        for (name, code, key) in KEYS {
            let c = chord(name);
            assert_eq!(c.code, *code, "{name}");
            assert_eq!(c.key, *key, "{name}");
        }
    }

    #[test]
    fn args_need_drive_and_read_the_script_up_front() {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-drive-args-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("s.txt");
        std::fs::write(&script, "idle\nstate\n").unwrap();
        let s = script.to_string_lossy().into_owned();
        let d = dir.to_string_lossy().into_owned();
        let args = |v: &[&str]| Args::parse(v.iter().map(|s| s.to_string()));

        assert!(args(&[&d]).unwrap().is_none());
        assert!(args(&[]).unwrap().is_none());
        assert!(
            args(&[&d, &d, "-NSDocumentRevisionsDebugMode", "YES"])
                .unwrap()
                .is_none(),
            "a normal launch with several paths or foreign flags is not a drive run"
        );
        let parsed = args(&["--drive", &s, "--drive-out", &d, &d])
            .unwrap()
            .unwrap();
        assert_eq!(parsed.steps, vec![Step::Idle, Step::State]);
        assert_eq!(parsed.out_dir, dir);
        assert_eq!(parsed.target, Some(dir.clone()));
        let parsed = args(&[&d, "--drive", &s]).unwrap().unwrap();
        assert_eq!(parsed.out_dir, PathBuf::from("."));

        assert!(args(&["--drive"]).is_err());
        let parsed = args(&["--drive", &s]).unwrap().unwrap();
        assert_eq!(parsed.target, None, "no path drives the home page");
        assert!(args(&["--drive-out", &d, &d]).is_err());
        assert!(args(&["--drive", &s, &d, &d]).is_err());
        assert!(args(&["--drive", &s, "/no/such/path"]).is_err());

        std::fs::write(&script, "idle\nbogus\n").unwrap();
        let err = args(&["--drive", &s, &d]).unwrap_err();
        assert!(err.ends_with("line 2: unknown step `bogus`"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_of_a_fresh_app_is_the_empty_grid() {
        let app = App::new(None);
        let json = serde_json::to_string(&State::of(&app)).unwrap();
        assert_eq!(
            json,
            r#"{"mode":"grid","visible":0,"grid_range":[0,0],"stacks":[],"sel":null,"selected":[],"photo":null,"focus":"folders","zoom":null,"status":null,"failed_thumbs":0,"folder_roots":[],"folder":null}"#
        );
    }
}
