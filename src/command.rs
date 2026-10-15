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

use crate::develop::{
    Adjustments, Crop, LocalAmounts, Mask, MaskShape, SliderId, Straighten, MAX_MASKS, SLIDERS,
    STRAIGHTEN_RANGE,
};
use crate::export::ExportSettings;
use crate::sidecar::{ColorLabel, ImageRecord, MAX_RATING};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "id", content = "params", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "folder.open")]
    OpenFolder(OpenFolderParams),
    #[serde(rename = "catalog.new")]
    NewCatalog(CatalogPathParams),
    #[serde(rename = "catalog.open")]
    OpenCatalog(CatalogPathParams),
    #[serde(rename = "catalog.switch")]
    SwitchCatalog(CatalogNameParams),
    #[serde(rename = "view.mode")]
    View(ViewParams),
    #[serde(rename = "view.zoom")]
    Zoom(ZoomParams),
    #[serde(rename = "view.filter")]
    Filter(FilterParams),
    #[serde(rename = "view.sort")]
    Sort(SortParams),
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
    #[serde(rename = "photo.score")]
    Score(PhotosParams),
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
    #[serde(rename = "mask.add")]
    AddMask(MaskAddParams),
    #[serde(rename = "mask.set")]
    SetMask(MaskSetParams),
    #[serde(rename = "mask.clear")]
    ClearMasks(MaskClearParams),
    #[serde(rename = "preset.apply")]
    Preset(PresetParams),
    #[serde(rename = "preset.add_folder")]
    AddPresetFolder(PresetFolderParams),
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

/// A catalog by path: a `<name>.lightphotos` folder, or a bare name, which
/// means that catalog in the app's catalog folder.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogPathParams {
    pub path: PathBuf,
}

/// A catalog by name: one opened before, or `default`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CatalogNameParams {
    pub name: String,
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

/// The Grid's filters. A filter left out stays as it is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct FilterParams {
    /// `null` clears the star filter.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub rating: Option<Option<RatingFilter>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<FlagShown>,
    /// The labels shown; an empty list clears the label filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<ColorLabel>>,
}

impl FilterParams {
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.rating.is_none() && self.flag.is_none() && self.labels.is_none() {
            return Err(CommandError::bad_params(
                "view.filter",
                "give rating, flag, labels or a mix".into(),
            ));
        }
        Ok(())
    }
}

/// A field that is present, even as `null`, as `Some`, so `null` can mean
/// clear while a missing field means leave it.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RatingFilter {
    #[serde(default)]
    pub cmp: StarCmp,
    pub stars: Stars,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StarCmp {
    #[default]
    #[serde(rename = ">=")]
    AtLeast,
    #[serde(rename = "=")]
    Exactly,
    #[serde(rename = "<=")]
    AtMost,
}

/// Which photos the Grid's flag filter shows.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlagShown {
    All,
    Picked,
    Unflagged,
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Name,
    Time,
    Modified,
    Size,
    Shutter,
    Quality,
    Rating,
    Edited,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SortParams {
    pub by: SortKey,
    /// Run against the key's natural direction. Name, time and shutter run
    /// A to Z, oldest and fastest first; the others run most first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverse: bool,
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

/// Runs `cmd` on the sidecar records of the photos it names, with the
/// result the app gives them, for a caller with no running app. The caller
/// resolves `photos` to records and saves each. A toggle is decided once
/// across all of them, as the app decides it: toggle pick picks every photo
/// unless every photo is picked already. `size_of(i)` gives photo `i`'s
/// pixel size, asked for only by a crop on a straightened photo. A command
/// that [`Command::about`] does not mark headless is refused with
/// [`CommandError::NeedsApp`]. A refused command, on any photo, leaves every
/// record as it was.
pub fn apply_to_records(
    cmd: &Command,
    records: &mut [ImageRecord],
    mut size_of: impl FnMut(usize) -> Result<(f32, f32), CommandError>,
) -> Result<(), CommandError> {
    if records.is_empty() {
        return Err(CommandError::NoPhotos);
    }
    let cmd = settle_toggles(cmd, records);
    let mut edited = records.to_vec();
    for (i, rec) in edited.iter_mut().enumerate() {
        apply_to_record(&cmd, rec, || size_of(i))?;
    }
    records.clone_from_slice(&edited);
    Ok(())
}

/// `cmd` with each toggle turned into the value the whole set takes.
fn settle_toggles(cmd: &Command, records: &[ImageRecord]) -> Command {
    use crate::sidecar::Flag;
    let mut cmd = cmd.clone();
    match &mut cmd {
        Command::Flag(p) if p.flag == FlagChange::TogglePick => {
            let all = records.iter().all(|r| r.flag == Some(Flag::Pick));
            p.flag = if all {
                FlagChange::None
            } else {
                FlagChange::Pick
            };
        }
        Command::Label(p) if p.on.is_none() => {
            p.on = Some(!records.iter().all(|r| r.label == Some(p.label)));
        }
        Command::Monochrome(p) if p.on.is_none() => {
            p.on = Some(!records.iter().all(|r| r.adjustments.is_monochrome()));
        }
        _ => {}
    }
    cmd
}

/// [`apply_to_records`] on one record, with any toggle already settled.
fn apply_to_record(
    cmd: &Command,
    rec: &mut ImageRecord,
    size: impl FnOnce() -> Result<(f32, f32), CommandError>,
) -> Result<(), CommandError> {
    use crate::sidecar::Flag;
    let now = crate::sidecar::unix_now();
    let adj = rec.adjustments;
    let develop = |rec: &mut ImageRecord, adj: Adjustments| rec.edit(now, |r| r.adjustments = adj);
    match cmd {
        Command::Rate(p) => rec.rating = Some(p.stars.get()).filter(|&n| n > 0),
        Command::Flag(p) => {
            rec.flag = match p.flag {
                FlagChange::Pick => Some(Flag::Pick),
                FlagChange::Reject => Some(Flag::Reject),
                FlagChange::None => None,
                FlagChange::TogglePick => (rec.flag != Some(Flag::Pick)).then_some(Flag::Pick),
            }
        }
        Command::Label(p) => {
            if p.on.unwrap_or(rec.label != Some(p.label)) {
                rec.label = Some(p.label);
            } else if rec.label == Some(p.label) {
                rec.label = None;
            }
        }
        Command::Rotate(p) => {
            let turn = if p.clockwise { 1 } else { 3 };
            rec.edit(now, |r| r.rotation = (r.rotation % 4 + turn) % 4);
        }
        Command::Develop(p) => {
            p.validate()?;
            develop(rec, p.apply(adj));
        }
        Command::Reset(_) => develop(rec, reset(adj)),
        Command::Monochrome(p) => {
            let on = p.on.unwrap_or(!adj.is_monochrome());
            let saturation = if on { -100.0 } else { 0.0 };
            develop(rec, Adjustments { saturation, ..adj });
        }
        Command::Crop(p) => {
            p.validate()?;
            develop(rec, p.apply(adj, size)?);
        }
        Command::AddMask(_) | Command::SetMask(_) | Command::ClearMasks(_) => {
            if let Some(masks) = cmd.edit_masks(&rec.masks) {
                let masks = masks?;
                rec.edit(now, |r| r.masks.set(masks));
            }
        }
        Command::OpenFolder(_)
        | Command::NewCatalog(_)
        | Command::OpenCatalog(_)
        | Command::SwitchCatalog(_)
        | Command::View(_)
        | Command::Zoom(_)
        | Command::Filter(_)
        | Command::Sort(_)
        | Command::Select(_)
        | Command::Navigate(_)
        | Command::AutoTone(_)
        | Command::Score(_)
        | Command::Preset(_)
        | Command::AddPresetFolder(_)
        | Command::Export(_)
        | Command::Undo => return Err(CommandError::NeedsApp),
    }
    Ok(())
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
    /// clears the crop, which on a turned photo is the largest crop that fits.
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
    /// [`validate`](Self::validate) first. A turned photo always keeps a box
    /// the turn fills, as the crop tool does: a cleared crop becomes the
    /// largest that fits, and a crop past the turned photo's edge is refused.
    /// `size` gives the photo's pixel size, asked for only when it is turned.
    pub fn apply(
        &self,
        mut adj: Adjustments,
        size: impl FnOnce() -> Result<(f32, f32), CommandError>,
    ) -> Result<Adjustments, CommandError> {
        if let Some(c) = self.crop {
            let full = c.left == 0.0 && c.top == 0.0 && c.right == 1.0 && c.bottom == 1.0;
            adj.crop = (!full).then_some(c);
        }
        if let Some(deg) = self.straighten {
            adj.straighten = deg;
        }
        if adj.straighten == 0.0 {
            return Ok(adj);
        }
        let (w, h) = size()?;
        if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
            return Err(CommandError::Failed {
                reason: format!("a straightened crop needs the photo's size, not {w}x{h}"),
            });
        }
        let turn = Straighten::new(adj.straighten, w, h);
        let Some(r) = adj.crop else {
            adj.crop = Some(turn.largest_crop(w / h));
            return Ok(adj);
        };
        if turn.covers(r) {
            return Ok(adj);
        }
        let ratio = (r.right - r.left) * w / ((r.bottom - r.top) * h);
        let fit = turn.largest_crop(if ratio.is_finite() && ratio > 0.0 {
            ratio
        } else {
            w / h
        });
        Err(CommandError::bad_params(
            "crop.set",
            format!(
                "a {}° straighten leaves the crop's corners past the photo's edge; \
                 the largest crop of its shape that fits is left {:.4}, top {:.4}, \
                 right {:.4}, bottom {:.4}",
                adj.straighten, fit.left, fit.top, fit.right, fit.bottom
            ),
        ))
    }
}

/// A new mask, added after a photo's others. `shape` is in 0..1 of the
/// unturned, uncropped photo; see [`MaskShape`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MaskAddParams {
    pub shape: MaskShape,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default, skip_serializing_if = "LocalAmounts::is_zero")]
    pub amounts: LocalAmounts,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

/// Changes mask `index` (0 is the first). Each field given replaces the
/// mask's own; `amounts` replaces all of them, so a slider it leaves out
/// goes to 0.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MaskSetParams {
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<MaskShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invert: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amounts: Option<LocalAmounts>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

/// Removes mask `index`, or every mask when `index` is absent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct MaskClearParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

/// A mask from a caller, held to range, or why it can't be one.
fn valid_mask(id: &str, mask: Mask) -> Result<Mask, CommandError> {
    mask.sanitized()
        .ok_or_else(|| CommandError::bad_params(id, "every mask number must be finite".to_string()))
}

impl MaskAddParams {
    fn apply(&self, masks: &[Mask]) -> Result<Vec<Mask>, CommandError> {
        if masks.len() >= MAX_MASKS {
            return Err(CommandError::bad_params(
                "mask.add",
                format!("a photo holds at most {MAX_MASKS} masks"),
            ));
        }
        let mask = Mask {
            shape: self.shape,
            invert: self.invert,
            amounts: self.amounts,
        };
        let mut out = masks.to_vec();
        out.push(valid_mask("mask.add", mask)?);
        Ok(out)
    }
}

impl MaskSetParams {
    fn apply(&self, masks: &[Mask]) -> Result<Vec<Mask>, CommandError> {
        let mut out = masks.to_vec();
        let slot = out
            .get_mut(self.index)
            .ok_or_else(|| no_mask("mask.set", self.index, masks.len()))?;
        let mask = Mask {
            shape: self.shape.unwrap_or(slot.shape),
            invert: self.invert.unwrap_or(slot.invert),
            amounts: self.amounts.unwrap_or(slot.amounts),
        };
        *slot = valid_mask("mask.set", mask)?;
        Ok(out)
    }
}

impl MaskClearParams {
    fn apply(&self, masks: &[Mask]) -> Result<Vec<Mask>, CommandError> {
        let Some(i) = self.index else {
            return Ok(Vec::new());
        };
        if i >= masks.len() {
            return Err(no_mask("mask.clear", i, masks.len()));
        }
        let mut out = masks.to_vec();
        out.remove(i);
        Ok(out)
    }
}

fn no_mask(id: &str, index: usize, len: usize) -> CommandError {
    CommandError::bad_params(id, format!("no mask {index}; the photo has {len} mask(s)"))
}

impl Command {
    /// `masks` as a mask command leaves them, or `None` for any other
    /// command. A caller works out every photo's result before writing any.
    pub fn edit_masks(&self, masks: &[Mask]) -> Option<Result<Vec<Mask>, CommandError>> {
        match self {
            Command::AddMask(p) => Some(p.apply(masks)),
            Command::SetMask(p) => Some(p.apply(masks)),
            Command::ClearMasks(p) => Some(p.apply(masks)),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetParams {
    pub name: String,
    /// The group a folder preset is listed under, for a name more than one
    /// preset folder holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Photos::is_selected")]
    pub photos: Photos,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetFolderParams {
    pub path: PathBuf,
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
            NewCatalog(_) => "catalog.new",
            OpenCatalog(_) => "catalog.open",
            SwitchCatalog(_) => "catalog.switch",
            View(_) => "view.mode",
            Zoom(_) => "view.zoom",
            Filter(_) => "view.filter",
            Sort(_) => "view.sort",
            Select(_) => "select",
            Navigate(_) => "navigate",
            Rate(_) => "photo.rate",
            Flag(_) => "photo.flag",
            Label(_) => "photo.label",
            Rotate(_) => "photo.rotate",
            Score(_) => "photo.score",
            Develop(_) => "develop.set",
            Reset(_) => "develop.reset",
            AutoTone(_) => "develop.auto",
            Monochrome(_) => "develop.monochrome",
            Crop(_) => "crop.set",
            AddMask(_) => "mask.add",
            SetMask(_) => "mask.set",
            ClearMasks(_) => "mask.clear",
            Preset(_) => "preset.apply",
            AddPresetFolder(_) => "preset.add_folder",
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
            Reset(p) | AutoTone(p) | Score(p) => Some(&p.photos),
            Monochrome(p) => Some(&p.photos),
            Crop(p) => Some(&p.photos),
            AddMask(p) => Some(&p.photos),
            SetMask(p) => Some(&p.photos),
            ClearMasks(p) => Some(&p.photos),
            Preset(p) => Some(&p.photos),
            Export(p) => Some(&p.photos),
            OpenFolder(_) | NewCatalog(_) | OpenCatalog(_) | SwitchCatalog(_) | View(_)
            | Zoom(_) | Filter(_) | Sort(_) | Select(_) | Navigate(_) | AddPresetFolder(_)
            | Undo => None,
        }
    }

    /// A one-line summary, and whether a caller with no running app can run
    /// it on a photo's sidecar.
    pub fn about(&self) -> (&'static str, bool) {
        use Command::*;
        match self {
            OpenFolder(_) => ("Open a folder in the Grid.", false),
            NewCatalog(_) => (
                "Make an empty catalog at `path` and open it. A bare name goes in the \
                 app's catalog folder.",
                false,
            ),
            OpenCatalog(_) => (
                "Open the catalog at `path`, a <name>.lightphotos folder.",
                false,
            ),
            SwitchCatalog(_) => (
                "Open a catalog by name: one opened before, or `default`.",
                false,
            ),
            View(_) => (
                "Show the Grid, the Loupe, or the Loupe with Develop open.",
                false,
            ),
            Zoom(_) => ("Zoom the Loupe: fit, actual, in, out or cycle.", false),
            Filter(_) => (
                "Set the Grid's filters. `rating` is {\"cmp\": \">=\"|\"=\"|\"<=\", \"stars\": \
                 0-5}, or null to clear; `flag` is all, picked, unflagged or rejected; \
                 `labels` lists the labels shown, empty to clear. A filter left out stays.",
                false,
            ),
            Sort(_) => (
                "Sort the Grid by name, time, modified, size, shutter, quality, rating or \
                 edited. Name, time and shutter run A to Z, oldest and fastest first, the \
                 others most first; `reverse` runs it the other way.",
                false,
            ),
            Select(_) => ("Select all, none, or the named photos.", false),
            Navigate(_) => ("Move to the next, previous, first or last photo.", false),
            Rate(_) => ("Set the star rating, 0 to 5; 0 clears it.", true),
            Flag(_) => ("Pick, reject, unflag, or toggle pick.", true),
            Label(_) => ("Set, clear or toggle a color label.", true),
            Rotate(_) => ("Rotate 90 degrees.", true),
            Score(_) => (
                "Score each photo's quality, as Actions > Score photos does. macOS only.",
                false,
            ),
            Develop(_) => (
                "Set develop controls. Put every control of one change in one call: \
                 it is one undo step.",
                true,
            ),
            Reset(_) => ("Reset every develop control, keeping the crop.", true),
            AutoTone(_) => ("Auto Tone the photos.", false),
            Monochrome(_) => ("Turn black and white on or off, or toggle it.", true),
            Crop(_) => ("Set the crop rectangle and the straighten angle.", true),
            AddMask(_) => (
                "Add a mask with its own tone and color sliders: a linear or radial \
                 gradient (points are 0..1 of the uncropped photo), or a detected one, \
                 {\"kind\":\"detected\",\"target\":\"subject\"|\"sky\"}, whose shape is \
                 found for each photo.",
                true,
            ),
            SetMask(_) => ("Change one mask's shape, invert or sliders.", true),
            ClearMasks(_) => ("Remove one mask, or every mask.", true),
            Preset(_) => (
                "Apply a preset by name: a saved one, or one from a preset folder. Give \
                 `group` when several preset folders hold that name.",
                false,
            ),
            AddPresetFolder(_) => (
                "Add a folder of Lightroom .xmp presets to the catalog's preset folders.",
                false,
            ),
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
            Command::NewCatalog(CatalogPathParams {
                path: PathBuf::from("/Users/me/Pictures/LightPhotos/Trip.lightphotos"),
            }),
            Command::OpenCatalog(CatalogPathParams {
                path: PathBuf::from("/Users/me/Pictures/LightPhotos/Trip.lightphotos"),
            }),
            Command::SwitchCatalog(CatalogNameParams {
                name: "default".into(),
            }),
            Command::View(ViewParams { mode: View::Loupe }),
            Command::Zoom(ZoomParams { zoom: Zoom::Fit }),
            Command::Filter(FilterParams {
                rating: Some(Some(RatingFilter {
                    cmp: StarCmp::AtLeast,
                    stars: Stars(3),
                })),
                flag: Some(FlagShown::Picked),
                labels: None,
            }),
            Command::Sort(SortParams {
                by: SortKey::Rating,
                reverse: false,
            }),
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
            Command::Score(PhotosParams { photos: some() }),
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
            Command::AddMask(MaskAddParams {
                shape: MaskShape::Radial {
                    center: [0.5, 0.4],
                    radii: [0.3, 0.2],
                    angle: 0.0,
                    feather: 0.5,
                },
                invert: false,
                amounts: LocalAmounts {
                    exposure: 0.5,
                    ..LocalAmounts::default()
                },
                photos: Photos::Selected,
            }),
            Command::SetMask(MaskSetParams {
                index: 0,
                shape: None,
                invert: Some(true),
                amounts: None,
                photos: Photos::Selected,
            }),
            Command::ClearMasks(MaskClearParams::default()),
            Command::Preset(PresetParams {
                name: "Golden".into(),
                group: None,
                photos: Photos::Selected,
            }),
            Command::AddPresetFolder(PresetFolderParams {
                path: PathBuf::from("/Users/me/Presets"),
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
            Command::parse(r#"{"id":"photo.score"}"#),
            Ok(Command::Score(PhotosParams::default()))
        );
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
    fn view_filter_tells_a_null_rating_from_a_missing_one() {
        let filter = |json: &str| match Command::parse(json) {
            Ok(Command::Filter(p)) => p,
            other => panic!("{other:?}"),
        };
        let p = filter(r#"{"id":"view.filter","params":{"rating":null}}"#);
        assert_eq!(p.rating, Some(None), "null clears");
        let p = filter(r#"{"id":"view.filter","params":{"flag":"rejected","labels":[]}}"#);
        assert_eq!(p.rating, None, "missing leaves it");
        assert_eq!(
            (p.flag, p.labels),
            (Some(FlagShown::Rejected), Some(vec![]))
        );
        let p = filter(r#"{"id":"view.filter","params":{"rating":{"stars":2}}}"#);
        assert_eq!(
            p.rating,
            Some(Some(RatingFilter {
                cmp: StarCmp::AtLeast,
                stars: Stars(2)
            })),
            "cmp defaults to >="
        );
        let wire = serde_json::to_value(Command::Filter(FilterParams {
            rating: Some(None),
            ..FilterParams::default()
        }))
        .unwrap();
        assert_eq!(
            wire,
            json!({"id": "view.filter", "params": {"rating": null}})
        );
        assert_eq!(
            Command::parse(r#"{"id":"view.sort","params":{"by":"edited","reverse":true}}"#),
            Ok(Command::Sort(SortParams {
                by: SortKey::Edited,
                reverse: true
            }))
        );
    }

    #[test]
    fn view_filter_and_sort_refuse_bad_values_with_an_example() {
        for text in [
            r#"{"id":"view.filter","params":{"rating":{"cmp":">=","stars":6}}}"#,
            r#"{"id":"view.filter","params":{"rating":{"cmp":">","stars":3}}}"#,
            r#"{"id":"view.filter","params":{"flag":"not_rejected"}}"#,
            r#"{"id":"view.filter","params":{"labels":["mauve"]}}"#,
            r#"{"id":"view.sort","params":{"by":"colour"}}"#,
            r#"{"id":"view.sort"}"#,
        ] {
            let Err(CommandError::BadParams { example, .. }) = Command::parse(text) else {
                panic!("{text} parsed");
            };
            assert!(example["params"].is_object(), "{text}");
        }
        let Err(CommandError::BadParams { reason, .. }) =
            Command::parse(r#"{"id":"view.filter","params":{"rating":{"stars":9}}}"#)
        else {
            panic!("stars 9 parsed");
        };
        assert!(reason.contains("stars run 0 to 5, not 9"), "{reason}");
        let Ok(Command::Filter(empty)) = Command::parse(r#"{"id":"view.filter"}"#) else {
            panic!("an empty filter parses, then fails validation");
        };
        assert!(matches!(
            empty.validate(),
            Err(CommandError::BadParams { .. })
        ));
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
        let unturned = || -> Result<(f32, f32), CommandError> { panic!("no size needed") };
        let full = crop(0.0, 0.0, 1.0, 1.0);
        let cropped = crop(0.1, 0.1, 0.9, 0.9)
            .apply(Adjustments::default(), unturned)
            .unwrap();
        assert_eq!(
            full.apply(cropped, unturned).unwrap().crop,
            None,
            "the full frame clears it"
        );
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
    fn a_turned_crop_set_keeps_a_box_the_turn_fills() {
        let size = || Ok((4000.0, 3000.0));
        let turn = Straighten::new(5.0, 4000.0, 3000.0);
        let tilt = |crop| CropParams {
            crop,
            straighten: Some(5.0),
            photos: Photos::Selected,
        };

        let adj = tilt(None).apply(Adjustments::default(), size).unwrap();
        assert_eq!(adj.crop, Some(turn.largest_crop(4000.0 / 3000.0)));
        let full = Crop {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        };
        let adj = tilt(Some(full))
            .apply(Adjustments::default(), size)
            .unwrap();
        assert_eq!(
            adj.crop,
            Some(turn.largest_crop(4000.0 / 3000.0)),
            "the full frame clears to the largest fitting crop"
        );

        let inner = Crop {
            left: 0.2,
            top: 0.2,
            right: 0.8,
            bottom: 0.8,
        };
        let adj = tilt(Some(inner))
            .apply(Adjustments::default(), size)
            .unwrap();
        assert_eq!(adj.crop, Some(inner));

        let wide = Crop {
            left: 0.01,
            top: 0.01,
            right: 0.99,
            bottom: 0.99,
        };
        let Err(CommandError::BadParams { reason, .. }) =
            tilt(Some(wide)).apply(Adjustments::default(), size)
        else {
            panic!("a crop past the turned edge is refused");
        };
        assert!(reason.contains("5° straighten"), "{reason}");
        assert!(reason.contains("largest crop"), "{reason}");

        let kept = Adjustments {
            crop: Some(wide),
            ..Default::default()
        };
        let turn_only = CropParams {
            crop: None,
            straighten: Some(5.0),
            photos: Photos::Selected,
        };
        assert!(
            turn_only.apply(kept, size).is_err(),
            "a kept crop must fit the new turn too"
        );
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

    fn on_record(rec: &mut ImageRecord, json: serde_json::Value) -> Result<(), CommandError> {
        let size = |_| Ok((4000.0, 3000.0));
        apply_to_records(&Command::from_json(&json)?, std::slice::from_mut(rec), size)
    }

    #[test]
    fn mask_commands_add_change_and_clear_a_records_masks() {
        let mut rec = ImageRecord::default();
        let add = json!({"id": "mask.add", "params": {
            "shape": {"kind": "linear", "start": [0.5, 0.0], "end": [0.5, 0.6]},
            "amounts": {"temp": -40}
        }});
        on_record(&mut rec, add.clone()).unwrap();
        on_record(&mut rec, add).unwrap();
        assert_eq!(rec.masks.len(), 2);
        assert_eq!(rec.masks[0].amounts.temp, -40.0);
        assert!(rec.edited.is_some(), "a mask is an edit");

        on_record(
            &mut rec,
            json!({"id": "mask.set", "params": {"index": 1, "invert": true}}),
        )
        .unwrap();
        assert!(rec.masks[1].invert && !rec.masks[0].invert);
        assert_eq!(rec.masks[1].amounts.temp, -40.0, "unnamed fields stay");

        let before = rec.clone();
        for bad in [
            json!({"id": "mask.set", "params": {"index": 5, "invert": true}}),
            json!({"id": "mask.clear", "params": {"index": 2}}),
            json!({"id": "mask.add", "params": {"shape": {"kind": "spiral"}}}),
        ] {
            assert!(on_record(&mut rec, bad.clone()).is_err(), "{bad}");
            assert_eq!(rec, before, "a refused {bad} changes nothing");
        }

        on_record(
            &mut rec,
            json!({"id": "mask.clear", "params": {"index": 0}}),
        )
        .unwrap();
        assert_eq!(rec.masks.len(), 1);
        on_record(&mut rec, json!({"id": "mask.clear", "params": {}})).unwrap();
        assert!(rec.masks.is_empty());
    }

    #[test]
    fn mask_add_refuses_past_max_masks() {
        let mut rec = ImageRecord::default();
        let add = json!({"id": "mask.add", "params": {
            "shape": {"kind": "radial", "center": [0.5, 0.5], "radii": [0.1, 0.1]}
        }});
        for _ in 0..MAX_MASKS {
            on_record(&mut rec, add.clone()).unwrap();
        }
        assert!(on_record(&mut rec, add).is_err());
        assert_eq!(rec.masks.len(), MAX_MASKS);
    }

    /// The listing's `headless` flag is the promise a headless caller reads,
    /// so it must name exactly the commands `apply_to_records` runs.
    #[test]
    fn apply_to_records_runs_exactly_the_commands_listed_as_headless() {
        for cmd in Command::examples() {
            let mut rec = ImageRecord::default();
            let ran = apply_to_records(&cmd, std::slice::from_mut(&mut rec), |_| {
                Ok((4000.0, 3000.0))
            });
            assert_eq!(
                ran != Err(CommandError::NeedsApp),
                cmd.about().1,
                "{}: {ran:?}",
                cmd.id()
            );
        }
    }

    #[test]
    fn ratings_flags_and_labels_toggle_on_a_record_as_the_keys_do() {
        let mut rec = ImageRecord::default();
        on_record(
            &mut rec,
            json!({"id": "photo.rate", "params": {"stars": 4}}),
        )
        .unwrap();
        assert_eq!(rec.rating, Some(4));
        on_record(
            &mut rec,
            json!({"id": "photo.rate", "params": {"stars": 0}}),
        )
        .unwrap();
        assert_eq!(rec.rating, None, "0 clears the rating");

        let toggle = json!({"id": "photo.flag", "params": {"flag": "toggle_pick"}});
        on_record(&mut rec, toggle.clone()).unwrap();
        assert_eq!(rec.flag, Some(crate::sidecar::Flag::Pick));
        on_record(&mut rec, toggle).unwrap();
        assert_eq!(rec.flag, None);

        let red = json!({"id": "photo.label", "params": {"label": "red"}});
        on_record(&mut rec, red.clone()).unwrap();
        assert_eq!(rec.label, Some(ColorLabel::Red));
        on_record(&mut rec, red).unwrap();
        assert_eq!(rec.label, None);
        rec.label = Some(ColorLabel::Blue);
        on_record(
            &mut rec,
            json!({"id": "photo.label", "params": {"label": "red", "on": false}}),
        )
        .unwrap();
        assert_eq!(
            rec.label,
            Some(ColorLabel::Blue),
            "off clears only that label"
        );
        assert_eq!(rec.edited, None, "culling is not an edit");
    }

    #[test]
    fn a_develop_edit_on_a_record_stamps_its_edit_time() {
        let mut rec = ImageRecord::default();
        on_record(
            &mut rec,
            json!({"id": "develop.set", "params": {"controls": {"exposure": 0.7}}}),
        )
        .unwrap();
        assert_eq!(rec.adjustments.exposure, 0.7);
        assert!(rec.edited.is_some());
    }

    #[test]
    fn a_refused_edit_leaves_the_record_as_it_was() {
        let mut rec = ImageRecord {
            rating: Some(2),
            ..ImageRecord::default()
        };
        let before = rec.clone();
        let err = on_record(
            &mut rec,
            json!({"id": "develop.set", "params": {"controls": {"exposure": 99}}}),
        );
        assert!(
            matches!(err, Err(CommandError::OutOfRange { .. })),
            "{err:?}"
        );
        let sizeless = apply_to_records(
            &Command::from_json(&json!({"id": "crop.set", "params": {"straighten": 3}})).unwrap(),
            std::slice::from_mut(&mut rec),
            |_| Ok((0.0, 0.0)),
        );
        assert!(
            matches!(sizeless, Err(CommandError::Failed { .. })),
            "{sizeless:?}"
        );
        assert_eq!(rec, before);
    }

    #[test]
    fn a_command_refused_on_one_record_changes_none() {
        let mut recs = vec![ImageRecord::default(); 2];
        let straighten =
            Command::from_json(&json!({"id": "crop.set", "params": {"straighten": 3}})).unwrap();
        let err = apply_to_records(&straighten, &mut recs, |i| {
            if i == 0 {
                Ok((4000.0, 3000.0))
            } else {
                Ok((0.0, 0.0))
            }
        });
        assert!(matches!(err, Err(CommandError::Failed { .. })), "{err:?}");
        assert_eq!(recs, vec![ImageRecord::default(); 2]);
        assert_eq!(
            apply_to_records(&straighten, &mut [], |_| Ok((1.0, 1.0))),
            Err(CommandError::NoPhotos)
        );
    }

    #[test]
    fn rotating_a_record_wraps_even_from_a_hostile_rotation() {
        let mut rec = ImageRecord {
            rotation: 255,
            ..ImageRecord::default()
        };
        let ccw = json!({"id": "photo.rotate", "params": {"clockwise": false}});
        on_record(&mut rec, ccw.clone()).unwrap();
        assert_eq!(rec.rotation, 2);
        on_record(&mut rec, ccw.clone()).unwrap();
        on_record(&mut rec, ccw).unwrap();
        assert_eq!(rec.rotation, 0);
        assert_eq!(rec.edited, None, "a full turn back is no edit");
    }

    #[test]
    fn a_reset_on_a_record_keeps_its_crop_straighten_and_spots() {
        let mut rec = ImageRecord::default();
        on_record(
            &mut rec,
            json!({"id": "crop.set", "params": {"straighten": 2}}),
        )
        .unwrap();
        rec.adjustments.exposure = 1.0;
        rec.touchups = vec![crate::develop::TouchUp {
            center: [0.5, 0.5],
            radius: 0.05,
            source: [0.3, 0.3],
            feather: 0.5,
            delta: [0.0; 3],
            opacity: 1.0,
        }];
        let kept = (rec.adjustments.crop, rec.touchups.clone());
        assert!(kept.0.is_some(), "a straighten picks the largest crop");

        on_record(&mut rec, json!({"id": "develop.reset"})).unwrap();
        assert_eq!(rec.adjustments.exposure, 0.0);
        assert_eq!(rec.adjustments.straighten, 2.0);
        assert_eq!((rec.adjustments.crop, rec.touchups), kept);
    }
}
