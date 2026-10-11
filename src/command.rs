// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every action an outside caller can run, as one serde enum. The keyboard,
//! the menu bar and `--drive` dispatch through [`Command`] in the app, and a
//! headless caller parses and lists the same type through the lib alone.
//!
//! The wire form is `{"id": "photo.rate", "params": {"stars": 4}}`. The serde
//! attributes are the contract: each variant's `rename` pins its id, and each
//! params struct's field names are its parameter names.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, Crop, SliderId, SLIDERS, STRAIGHTEN_RANGE};
use crate::export::ExportSettings;
use crate::sidecar::{ColorLabel, MAX_RATING};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "id", content = "params", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "folder.open")]
    OpenFolder(OpenFolderParams),
    #[serde(rename = "view.mode")]
    View(ViewParams),
    #[serde(rename = "view.zoom")]
    Zoom(ZoomParams),
    #[serde(rename = "select")]
    Select(SelectParams),
    #[serde(rename = "navigate")]
    Navigate(NavigateParams),
    #[serde(rename = "photo.rate")]
    Rate(RateParams),
    #[serde(rename = "photo.flag")]
    Flag(FlagParams),
    #[serde(rename = "photo.label")]
    Label(LabelParams),
    #[serde(rename = "photo.rotate")]
    Rotate(RotateParams),
    #[serde(rename = "develop.set")]
    Develop(DevelopParams),
    #[serde(rename = "develop.reset")]
    Reset(PhotosParams),
    #[serde(rename = "develop.auto")]
    AutoTone(PhotosParams),
    #[serde(rename = "develop.monochrome")]
    Monochrome(MonochromeParams),
    #[serde(rename = "crop.set")]
    Crop(CropParams),
    #[serde(rename = "preset.apply")]
    Preset(PresetParams),
    #[serde(rename = "export")]
    Export(ExportParams),
    #[serde(rename = "edit.undo")]
    Undo,
}

/// Which photos a command acts on. Absent means what the keyboard acts on:
/// the selection, or the Compare pane's picks. Each name is an absolute
/// path, or a file name matched among the open folder's photos.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(untagged)]
pub enum Photos {
    #[default]
    Selected,
    Named(Vec<PathBuf>),
}

impl Photos {
    pub fn is_selected(&self) -> bool {
        *self == Photos::Selected
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct PhotosParams {
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OpenFolderParams {
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Grid,
    Loupe,
    /// The Loupe with the Develop sliders open.
    Develop,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ViewParams {
    pub mode: View,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Zoom {
    Fit,
    /// 100%: one image pixel per screen pixel.
    Actual,
    In,
    Out,
    /// Fit, then 2x fit, then 100%, as Space does.
    Cycle,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ZoomParams {
    pub zoom: Zoom,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectWord {
    All,
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(untagged)]
pub enum Selection {
    Word(SelectWord),
    Named(Vec<PathBuf>),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SelectParams {
    pub photos: Selection,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Next,
    Previous,
    First,
    Last,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NavigateParams {
    pub to: Step,
}

/// A star rating, `0..=MAX_RATING`, checked when parsed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(try_from = "u8", into = "u8")]
pub struct Stars(u8);

impl Stars {
    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Stars {
    type Error = String;

    fn try_from(n: u8) -> Result<Stars, String> {
        if n <= MAX_RATING {
            Ok(Stars(n))
        } else {
            Err(format!("stars run 0 to {MAX_RATING}, not {n}"))
        }
    }
}

impl From<Stars> for u8 {
    fn from(s: Stars) -> u8 {
        s.0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RateParams {
    pub stars: Stars,
    /// Move to the next photo after rating one, as Shift+digit does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub advance: bool,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlagChange {
    Pick,
    Reject,
    None,
    /// Pick, or unflag when every photo is already picked, as ` does.
    TogglePick,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FlagParams {
    pub flag: FlagChange,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LabelParams {
    pub label: ColorLabel,
    /// Absent toggles: the label goes on, or comes off when every photo has
    /// it, as 6-9 do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RotateParams {
    pub clockwise: bool,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

/// `0.7` sets a control. `{"by": 5}` moves it and clamps it to its range (a
/// grading hue wraps), as a keyboard nudge does. `"default"` resets it, as a
/// double-click does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(untagged)]
pub enum ControlValue {
    To(f32),
    By { by: f32 },
    Default(DefaultWord),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DefaultWord {
    Default,
}

/// Any number of develop controls, set together as one undo step.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DevelopParams {
    pub controls: BTreeMap<SliderId, ControlValue>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

impl DevelopParams {
    /// Checks every value before any photo changes, so one bad control
    /// leaves every photo as it was. A `To` outside the range is refused, not
    /// clamped: a wrong guess is reported rather than becoming another edit.
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.controls.is_empty() {
            return Err(self.bad("controls is empty; name at least one control"));
        }
        for (&id, &value) in &self.controls {
            let slider = id.slider().ok_or_else(|| self.bad("unknown control"))?;
            let (min, max) = (*slider.range.start(), *slider.range.end());
            match value {
                ControlValue::To(v) if !v.is_finite() || v < min || v > max => {
                    return Err(CommandError::OutOfRange {
                        control: id.to_string(),
                        value: v,
                        min,
                        max,
                    });
                }
                ControlValue::By { by } if !by.is_finite() => {
                    return Err(self.bad(&format!("{id}: by must be a finite number")));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `adj` with every control set. Call [`validate`](Self::validate) first.
    pub fn apply(&self, mut adj: Adjustments) -> Adjustments {
        for (&id, &value) in &self.controls {
            let Some(slider) = id.slider() else {
                continue;
            };
            let field = (slider.field)(&mut adj);
            *field = match value {
                ControlValue::To(v) => v.clamp(*slider.range.start(), *slider.range.end()),
                ControlValue::By { by } => slider.stepped(*field, by / slider.step),
                ControlValue::Default(_) => slider.default,
            };
        }
        adj
    }

    fn bad(&self, reason: &str) -> CommandError {
        CommandError::bad_params("develop.set", reason.to_string())
    }
}

/// Every control back to its default, keeping the crop and straighten.
pub fn reset(adj: Adjustments) -> Adjustments {
    Adjustments {
        crop: adj.crop,
        straighten: adj.straighten,
        ..Adjustments::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MonochromeParams {
    /// Absent toggles: black and white, or back to color when every photo
    /// already is, as V does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CropParams {
    /// 0..1 of the straightened photo, origin top-left. The full frame
    /// clears the crop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Degrees, within `STRAIGHTEN_RANGE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straighten: Option<f32>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

impl CropParams {
    pub fn validate(&self) -> Result<(), CommandError> {
        let bad = |reason: &str| CommandError::bad_params("crop.set", reason.to_string());
        if self.crop.is_none() && self.straighten.is_none() {
            return Err(bad("give crop, straighten or both"));
        }
        if let Some(c) = self.crop {
            let edges = [c.left, c.top, c.right, c.bottom];
            if edges
                .iter()
                .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
            {
                return Err(bad("crop edges run 0 to 1"));
            }
            if c.left >= c.right || c.top >= c.bottom {
                return Err(bad("crop needs left < right and top < bottom"));
            }
        }
        if let Some(deg) = self.straighten {
            let (min, max) = (*STRAIGHTEN_RANGE.start(), *STRAIGHTEN_RANGE.end());
            if !deg.is_finite() || deg < min || deg > max {
                return Err(CommandError::OutOfRange {
                    control: "straighten".into(),
                    value: deg,
                    min,
                    max,
                });
            }
        }
        Ok(())
    }

    /// `adj` with the crop and straighten set. Call
    /// [`validate`](Self::validate) first.
    pub fn apply(&self, mut adj: Adjustments) -> Adjustments {
        if let Some(c) = self.crop {
            let full = c.left == 0.0 && c.top == 0.0 && c.right == 1.0 && c.bottom == 1.0;
            adj.crop = (!full).then_some(c);
        }
        if let Some(deg) = self.straighten {
            adj.straighten = deg;
        }
        adj
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ExportParams {
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
    /// Used for this one export and not saved. Absent uses the export
    /// form's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<ExportSettings>,
}

/// What a caller gets back when a command runs.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// How many photos the command acted on.
    pub photos: usize,
    /// A background job started (export, Auto Tone of several photos).
    /// Poll the app's state for its end.
    pub pending: bool,
}

/// What holds the app's input, so a command cannot run now.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    Tour,
    Confirm,
    CopySettings,
    Crop,
    Straighten,
    WbPicker,
    TouchUp,
    PresetName,
    Settings,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum CommandError {
    /// The request is not a JSON object with a string `id`.
    BadJson {
        reason: String,
    },
    UnknownCommand {
        id: String,
    },
    BadParams {
        id: String,
        reason: String,
        example: serde_json::Value,
    },
    OutOfRange {
        control: String,
        value: f32,
        min: f32,
        max: f32,
    },
    /// The selection is empty, or the command needs a photo on screen.
    NoPhotos,
    NoSuchPhoto {
        name: String,
    },
    Busy {
        by: Blocker,
    },
    /// The command acts on what the app shows, and the app is in another view.
    NeedsView {
        view: View,
    },
    /// A headless caller asked for a command only the running app can do.
    NeedsApp,
    Failed {
        reason: String,
    },
}

impl CommandError {
    fn bad_params(id: &str, reason: String) -> CommandError {
        let example = Command::examples()
            .into_iter()
            .find(|c| c.id() == id)
            .and_then(|c| serde_json::to_value(c).ok())
            .unwrap_or_default();
        CommandError::BadParams {
            id: id.to_string(),
            reason,
            example,
        }
    }

    /// True for an error that only means there was nothing to act on, which
    /// a key press shrugs off.
    pub fn is_nothing_to_do(&self) -> bool {
        matches!(
            self,
            CommandError::NoPhotos | CommandError::Busy { .. } | CommandError::NeedsView { .. }
        )
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::BadJson { reason } => write!(
                f,
                "not a command: {reason}; send {{\"id\": \"…\", \"params\": {{…}}}}"
            ),
            CommandError::UnknownCommand { id } => {
                write!(f, "no command `{id}`; list the commands to see every id")
            }
            CommandError::BadParams {
                id,
                reason,
                example,
            } => write!(f, "bad params for `{id}`: {reason}; for example {example}"),
            CommandError::OutOfRange {
                control,
                value,
                min,
                max,
            } => write!(f, "{control} runs {min} to {max}, not {value}"),
            CommandError::NoPhotos => {
                write!(f, "no photos to act on; select some or pass `photos`")
            }
            CommandError::NoSuchPhoto { name } => {
                write!(f, "no photo `{name}` in the open folder")
            }
            CommandError::Busy { by } => write!(
                f,
                "the app is busy with {}; finish it with Enter or Esc first",
                serde_json::to_value(by)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ),
            CommandError::NeedsView { view } => write!(
                f,
                "this works in the {} view; run view.mode first",
                serde_json::to_value(view)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ),
            CommandError::NeedsApp => write!(f, "this command needs the running app"),
            CommandError::Failed { reason } => f.write_str(reason),
        }
    }
}

impl std::error::Error for CommandError {}

impl Command {
    /// The command's wire id, such as `photo.rate`.
    pub fn id(&self) -> &'static str {
        use Command::*;
        match self {
            OpenFolder(_) => "folder.open",
            View(_) => "view.mode",
            Zoom(_) => "view.zoom",
            Select(_) => "select",
            Navigate(_) => "navigate",
            Rate(_) => "photo.rate",
            Flag(_) => "photo.flag",
            Label(_) => "photo.label",
            Rotate(_) => "photo.rotate",
            Develop(_) => "develop.set",
            Reset(_) => "develop.reset",
            AutoTone(_) => "develop.auto",
            Monochrome(_) => "develop.monochrome",
            Crop(_) => "crop.set",
            Preset(_) => "preset.apply",
            Export(_) => "export",
            Undo => "edit.undo",
        }
    }

    /// The photos a command acts on, for the commands that take `photos`.
    pub fn photos(&self) -> Option<&Photos> {
        use Command::*;
        match self {
            Rate(p) => Some(&p.photos),
            Flag(p) => Some(&p.photos),
            Label(p) => Some(&p.photos),
            Rotate(p) => Some(&p.photos),
            Develop(p) => Some(&p.photos),
            Reset(p) | AutoTone(p) => Some(&p.photos),
            Monochrome(p) => Some(&p.photos),
            Crop(p) => Some(&p.photos),
            Preset(p) => Some(&p.photos),
            Export(p) => Some(&p.photos),
            OpenFolder(_) | View(_) | Zoom(_) | Select(_) | Navigate(_) | Undo => None,
        }
    }

    /// A one-line summary, and whether a caller with no running app can run
    /// it on a photo's sidecar.
    pub fn about(&self) -> (&'static str, bool) {
        use Command::*;
        match self {
            OpenFolder(_) => ("Open a folder in the Grid.", false),
            View(_) => (
                "Show the Grid, the Loupe, or the Loupe with Develop open.",
                false,
            ),
            Zoom(_) => ("Zoom the Loupe: fit, actual, in, out or cycle.", false),
            Select(_) => ("Select all, none, or the named photos.", false),
            Navigate(_) => ("Move to the next, previous, first or last photo.", false),
            Rate(_) => ("Set the star rating, 0 to 5; 0 clears it.", true),
            Flag(_) => ("Pick, reject, unflag, or toggle pick.", true),
            Label(_) => ("Set, clear or toggle a color label.", true),
            Rotate(_) => ("Rotate 90 degrees.", true),
            Develop(_) => (
                "Set develop controls. Put every control of one change in one call: \
                 it is one undo step.",
                true,
            ),
            Reset(_) => ("Reset every develop control, keeping the crop.", true),
            AutoTone(_) => ("Auto Tone the photos.", false),
            Monochrome(_) => ("Turn black and white on or off, or toggle it.", true),
            Crop(_) => ("Set the crop rectangle and the straighten angle.", true),
            Preset(_) => ("Apply a saved preset by name.", false),
            Export(_) => (
                "Export with the form's settings, or with `settings` for this run.",
                false,
            ),
            Undo => ("Undo the last develop change.", false),
        }
    }

    /// One example of every command. A test proves none is missing.
    pub fn examples() -> Vec<Command> {
        let some = || Photos::Named(vec![PathBuf::from("IMG_0042.ARW")]);
        let mut controls = BTreeMap::new();
        controls.insert(SliderId::Exposure, ControlValue::To(0.7));
        controls.insert(SliderId::Contrast, ControlValue::By { by: 5.0 });
        controls.insert(
            SliderId::VignetteAmount,
            ControlValue::Default(DefaultWord::Default),
        );
        vec![
            Command::OpenFolder(OpenFolderParams {
                path: PathBuf::from("/Users/me/Pictures/Trip"),
            }),
            Command::View(ViewParams { mode: View::Loupe }),
            Command::Zoom(ZoomParams { zoom: Zoom::Fit }),
            Command::Select(SelectParams {
                photos: Selection::Word(SelectWord::All),
            }),
            Command::Navigate(NavigateParams { to: Step::Next }),
            Command::Rate(RateParams {
                stars: Stars(4),
                advance: false,
                photos: Photos::Selected,
            }),
            Command::Flag(FlagParams {
                flag: FlagChange::Pick,
                photos: some(),
            }),
            Command::Label(LabelParams {
                label: ColorLabel::Red,
                on: Some(true),
                photos: Photos::Selected,
            }),
            Command::Rotate(RotateParams {
                clockwise: true,
                photos: Photos::Selected,
            }),
            Command::Develop(DevelopParams {
                controls,
                photos: Photos::Selected,
            }),
            Command::Reset(PhotosParams::default()),
            Command::AutoTone(PhotosParams::default()),
            Command::Monochrome(MonochromeParams {
                on: Some(true),
                photos: Photos::Selected,
            }),
            Command::Crop(CropParams {
                crop: Some(Crop {
                    left: 0.1,
                    top: 0.1,
                    right: 0.9,
                    bottom: 0.9,
                }),
                straighten: Some(1.5),
                photos: Photos::Selected,
            }),
            Command::Preset(PresetParams {
                name: "Golden".into(),
                photos: Photos::Selected,
            }),
            Command::Export(ExportParams::default()),
            Command::Undo,
        ]
    }

    /// Parses one request line.
    pub fn parse(text: &str) -> Result<Command, CommandError> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| CommandError::BadJson {
                reason: e.to_string(),
            })?;
        Command::from_json(&value)
    }

    /// Parses one request. An unknown id and bad params give different
    /// errors, and bad params come back with an example.
    pub fn from_json(value: &serde_json::Value) -> Result<Command, CommandError> {
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CommandError::BadJson {
                reason: "missing a string `id`".into(),
            })?;
        let example = Command::examples()
            .into_iter()
            .find(|c| c.id() == id)
            .ok_or_else(|| CommandError::UnknownCommand { id: id.to_string() })?;
        let bad = |e: serde_json::Error| CommandError::bad_params(id, e.to_string());
        // serde wants `params` on every command that has a params struct, so
        // a request that leaves out params whose fields all have defaults,
        // such as `{"id": "develop.reset"}`, gets an empty object.
        let wants_params = serde_json::to_value(&example)
            .map_err(bad)?
            .get("params")
            .is_some();
        match value {
            serde_json::Value::Object(fields) if wants_params && !fields.contains_key("params") => {
                let mut fields = fields.clone();
                fields.insert("params".into(), serde_json::json!({}));
                Command::deserialize(serde_json::Value::Object(fields)).map_err(bad)
            }
            _ => Command::deserialize(value).map_err(bad),
        }
    }
}

/// What the `commands` request returns.
#[derive(Serialize, Debug)]
pub struct Listing {
    pub commands: Vec<CommandInfo>,
    pub controls: Vec<ControlInfo>,
}

#[derive(Serialize, Debug)]
pub struct CommandInfo {
    pub id: &'static str,
    pub summary: &'static str,
    pub headless: bool,
    /// A real command, serialized, so its field names cannot go stale.
    pub example: serde_json::Value,
}

#[derive(Serialize, Debug)]
pub struct ControlInfo {
    pub id: String,
    pub section: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub step: f32,
    pub decimals: usize,
    /// The value on `current`, or `None` with no photo.
    pub value: Option<f32>,
}

/// Every command and every develop control. `current` is the edit of the
/// photo the app shows, or of the photo a headless caller named.
pub fn listing(current: Option<&Adjustments>) -> Listing {
    let commands = Command::examples()
        .into_iter()
        .map(|cmd| {
            let (summary, headless) = cmd.about();
            CommandInfo {
                id: cmd.id(),
                summary,
                headless,
                example: serde_json::to_value(&cmd).unwrap_or_default(),
            }
        })
        .collect();
    let controls = SLIDERS
        .iter()
        .map(|s| ControlInfo {
            id: s.id.to_string(),
            section: s.section.name(),
            min: *s.range.start(),
            max: *s.range.end(),
            default: s.default,
            step: s.step,
            decimals: s.decimals,
            value: current.map(|adj| {
                let mut adj = *adj;
                *(s.field)(&mut adj)
            }),
        })
        .collect();
    Listing { commands, controls }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_example_round_trips_through_the_parser() {
        for cmd in Command::examples() {
            let wire = serde_json::to_value(&cmd).unwrap();
            assert_eq!(wire["id"], cmd.id(), "the serde tag is the id");
            assert_eq!(Command::from_json(&wire), Ok(cmd), "{wire}");
        }
    }

    /// serde lists every variant tag when it meets an unknown one, so this
    /// catches a variant added without an example.
    #[test]
    fn every_command_has_an_example() {
        let err = serde_json::from_value::<Command>(json!({"id": "?"}))
            .unwrap_err()
            .to_string();
        let tags: Vec<&str> = err.split('`').skip(3).step_by(2).collect();
        assert!(tags.len() > 10, "serde's message changed: {err}");
        let ids: Vec<&str> = Command::examples().iter().map(Command::id).collect();
        for tag in &tags {
            assert!(ids.contains(tag), "{tag} has no example: {err}");
        }
        assert_eq!(tags.len(), ids.len(), "one example per command");
    }

    #[test]
    fn ids_are_unique() {
        let ids: std::collections::BTreeSet<&str> =
            Command::examples().iter().map(Command::id).collect();
        assert_eq!(ids.len(), Command::examples().len());
    }

    #[test]
    fn the_wire_form_parses_as_documented() {
        let cmd = Command::parse(
            r#"{"id":"develop.set","params":{"controls":{"exposure":0.7,"contrast":20,
               "mix.blue.saturation":-30,"clarity":{"by":5},"vignette.amount":"default"}}}"#,
        )
        .unwrap();
        let Command::Develop(p) = cmd else {
            panic!("{cmd:?}");
        };
        assert_eq!(p.photos, Photos::Selected);
        assert_eq!(p.controls[&SliderId::Exposure], ControlValue::To(0.7));
        assert_eq!(p.controls[&SliderId::Clarity], ControlValue::By { by: 5.0 });
        assert_eq!(
            p.controls[&SliderId::VignetteAmount],
            ControlValue::Default(DefaultWord::Default)
        );
        assert_eq!(Command::parse(r#"{"id":"edit.undo"}"#), Ok(Command::Undo));
        assert_eq!(
            Command::parse(r#"{"id":"develop.reset"}"#),
            Ok(Command::Reset(PhotosParams::default())),
            "params with only defaults may be left out"
        );
        assert_eq!(
            Command::parse(r#"{"id":"photo.flag","params":{"flag":"pick","photos":["a.jpg"]}}"#),
            Ok(Command::Flag(FlagParams {
                flag: FlagChange::Pick,
                photos: Photos::Named(vec!["a.jpg".into()]),
            }))
        );
    }

    #[test]
    fn malformed_requests_are_errors_not_panics() {
        for text in [
            "",
            "{",
            "null",
            "[]",
            "42",
            r#"{"params":{}}"#,
            r#"{"id":7}"#,
            r#"{"id":"photo.rate"}"#,
            r#"{"id":"photo.rate","params":{"stars":6}}"#,
            r#"{"id":"photo.rate","params":{"stars":-1}}"#,
            r#"{"id":"photo.rate","params":{"stars":2,"bogus":1}}"#,
            r#"{"id":"photo.rate","params":{"stars":2},"extra":1}"#,
            r#"{"id":"develop.set","params":{"controls":{"exposur":1}}}"#,
            r#"{"id":"develop.set","params":{"controls":{"exposure":1e39}}}"#,
            r#"{"id":"develop.set","params":{"controls":{"exposure":{"by":"x"}}}}"#,
            r#"{"id":"crop.set","params":{"crop":{"left":0}}}"#,
            r#"{"id":"select","params":{"photos":"some"}}"#,
            "{\"id\":\"photo.label\",\"params\":{\"label\":\"\u{1F600}\"}}",
        ] {
            let parsed = Command::parse(text);
            let checked = match &parsed {
                Ok(Command::Develop(p)) => p.validate(),
                Ok(_) => Err(CommandError::NoPhotos),
                Err(e) => Err(e.clone()),
            };
            assert!(checked.is_err(), "{text:?} gave {parsed:?}");
        }
    }

    #[test]
    fn an_unknown_id_and_bad_params_are_told_apart() {
        assert_eq!(
            Command::parse(r#"{"id":"photo.rat"}"#),
            Err(CommandError::UnknownCommand {
                id: "photo.rat".into()
            })
        );
        let Err(CommandError::BadParams { id, example, .. }) =
            Command::parse(r#"{"id":"photo.rate","params":{"stars":9}}"#)
        else {
            panic!("expected BadParams");
        };
        assert_eq!(id, "photo.rate");
        assert_eq!(example["id"], "photo.rate", "the error carries an example");
    }

    #[test]
    fn develop_set_refuses_an_out_of_range_value_and_names_the_range() {
        let p = DevelopParams {
            controls: BTreeMap::from([
                (SliderId::Contrast, ControlValue::To(20.0)),
                (SliderId::Exposure, ControlValue::To(9.0)),
            ]),
            photos: Photos::Selected,
        };
        assert_eq!(
            p.validate(),
            Err(CommandError::OutOfRange {
                control: "exposure".into(),
                value: 9.0,
                min: -5.0,
                max: 5.0
            })
        );
    }

    #[test]
    fn develop_set_moves_by_a_step_and_resets_to_default() {
        let p = DevelopParams {
            controls: BTreeMap::from([
                (SliderId::Contrast, ControlValue::By { by: 500.0 }),
                (SliderId::Exposure, ControlValue::To(-1.25)),
                (
                    SliderId::VignetteMidpoint,
                    ControlValue::Default(DefaultWord::Default),
                ),
                (
                    SliderId::Grade(crate::develop::Zone::Global, crate::develop::ColorAxis::Hue),
                    ControlValue::By { by: 370.0 },
                ),
            ]),
            photos: Photos::Selected,
        };
        p.validate().unwrap();
        let mut start = Adjustments::default();
        start.vignette.midpoint = 10.0;
        let adj = p.apply(start);
        assert_eq!(adj.contrast, 100.0, "by clamps to the range");
        assert_eq!(adj.exposure, -1.25);
        assert_eq!(adj.vignette.midpoint, 50.0, "default is the slider's own");
        assert!((adj.grade.global.hue - 10.0).abs() < 1e-3, "a hue wraps");
    }

    #[test]
    fn crop_set_checks_its_rectangle_and_angle() {
        let crop = |l, t, r, b| CropParams {
            crop: Some(Crop {
                left: l,
                top: t,
                right: r,
                bottom: b,
            }),
            straighten: None,
            photos: Photos::Selected,
        };
        assert!(crop(0.1, 0.1, 0.9, 0.9).validate().is_ok());
        assert!(crop(0.9, 0.1, 0.1, 0.9).validate().is_err());
        assert!(crop(-0.1, 0.0, 1.0, 1.0).validate().is_err());
        assert!(crop(0.0, 0.0, f32::NAN, 1.0).validate().is_err());
        let full = crop(0.0, 0.0, 1.0, 1.0);
        let cropped = crop(0.1, 0.1, 0.9, 0.9).apply(Adjustments::default());
        assert_eq!(full.apply(cropped).crop, None, "the full frame clears it");
        let tilt = CropParams {
            crop: None,
            straighten: Some(60.0),
            photos: Photos::Selected,
        };
        assert!(matches!(
            tilt.validate(),
            Err(CommandError::OutOfRange { .. })
        ));
    }

    #[test]
    fn the_listing_names_every_command_and_control_with_current_values() {
        let adj = Adjustments {
            exposure: 0.5,
            ..Default::default()
        };
        let listing = listing(Some(&adj));
        assert_eq!(listing.commands.len(), Command::examples().len());
        assert_eq!(listing.controls.len(), SLIDERS.len());
        let exposure = listing
            .controls
            .iter()
            .find(|c| c.id == "exposure")
            .unwrap();
        assert_eq!(
            (exposure.min, exposure.max, exposure.default, exposure.value),
            (-5.0, 5.0, 0.0, Some(0.5))
        );
        let midpoint = listing
            .controls
            .iter()
            .find(|c| c.id == "vignette.midpoint")
            .unwrap();
        assert_eq!(midpoint.default, 50.0);
        assert!(super::listing(None)
            .controls
            .iter()
            .all(|c| c.value.is_none()));
        let json = serde_json::to_value(&listing).unwrap();
        assert_eq!(json["commands"][0]["id"], "folder.open");
    }
}
