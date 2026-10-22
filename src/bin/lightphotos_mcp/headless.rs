//! Headless mode: commands edit the photos' sidecars, and `render` decodes,
//! develops and encodes through the lib's export path. No app runs.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};

use lightphotos::command::{self, Command, CommandError, Photos};
use lightphotos::decode::image_decode;
use lightphotos::export::{self, Output};
use lightphotos::sidecar::{self, ImageRecord};

use crate::tools::{command_request, image, text, Tool, ToolResult};

pub const MIN_RENDER: u32 = 16;
pub const MAX_RENDER: u32 = 4096;
pub const DEFAULT_RENDER: u32 = 1024;
/// `get_state` lists at most this many photos.
const MAX_LISTED: usize = 2000;

pub struct Folder {
    /// Canonical, so a path an agent names can be compared with it.
    root: PathBuf,
}

impl Folder {
    pub fn open(dir: &Path) -> Result<Folder, String> {
        let root = dir
            .canonicalize()
            .map_err(|e| format!("cannot open the folder {}: {e}", dir.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a folder", dir.display()));
        }
        Ok(Folder { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn call(&self, tool: Tool, args: &Map<String, Value>) -> ToolResult {
        let photo_arg = || args.get("photo").and_then(Value::as_str);
        match tool {
            Tool::ListCommands => {
                let current = match photo_arg() {
                    Some(name) => Some(self.record(&self.photo(Path::new(name))?)?.adjustments),
                    None => None,
                };
                let listing = serde_json::to_string(&command::listing(current.as_ref()))
                    .map_err(|e| e.to_string())?;
                Ok(vec![text(listing)])
            }
            Tool::RunCommand => self.run(args),
            Tool::GetState | Tool::ListPhotos => Ok(vec![text(self.state()?.to_string())]),
            Tool::Screenshot => Err("screenshot needs the running app. Headless, call render \
                                     to see a photo with its edits."
                .into()),
            Tool::Render => {
                let name = photo_arg().ok_or("pass `photo`, a file name from get_state")?;
                let max = match args.get("max_size") {
                    None | Some(Value::Null) => DEFAULT_RENDER,
                    Some(v) => v
                        .as_u64()
                        .map(|n| {
                            u32::try_from(n)
                                .unwrap_or(MAX_RENDER)
                                .clamp(MIN_RENDER, MAX_RENDER)
                        })
                        .ok_or("`max_size` must be a whole number of pixels")?,
                };
                self.render(Path::new(name), max)
            }
        }
    }

    /// Runs one headless command on the photos it names. Every photo's
    /// sidecar is read and edited before any is written.
    fn run(&self, args: &Map<String, Value>) -> ToolResult {
        let cmd = Command::from_json(&command_request(args)?).map_err(explain)?;
        if !cmd.about().1 {
            return Err(explain(CommandError::NeedsApp));
        }
        let names = match cmd.photos() {
            Some(Photos::Named(names)) if !names.is_empty() => names,
            _ => {
                return Err(format!(
                    "pass params.photos, a list of file names in {}, such as [\"IMG_0042.JPG\"]; \
                     headless there is no selection",
                    self.root.display()
                ))
            }
        };
        let mut seen = HashSet::new();
        let mut paths = Vec::new();
        for name in names {
            let path = self.photo(name)?;
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
        // One writer per folder: hold the lock from the read to the last
        // write, and leave a folder the app has open to the app.
        let _lock = self.lock()?;
        let mut records = paths
            .iter()
            .map(|p| self.record(p))
            .collect::<Result<Vec<_>, _>>()?;
        command::apply_to_records(&cmd, &mut records, |i| {
            paths
                .get(i)
                .map_or(Err(CommandError::NoPhotos), |p| display_size(p))
        })
        .map_err(explain)?;
        let mut notes = Vec::new();
        for (path, rec) in paths.iter().zip(&mut records) {
            notes.extend(detect_for(path, rec));
        }
        for (n, (path, rec)) in paths.iter().zip(&records).enumerate() {
            sidecar::write_record(path, rec).map_err(|e| {
                format!(
                    "saved {n} of {} photos, then could not save {}: {e}",
                    paths.len(),
                    file_name(path)
                )
            })?;
        }
        let photos: Vec<Value> = paths
            .iter()
            .zip(&records)
            .map(|(p, r)| entry(p, r))
            .collect();
        let mut reply = json!({ "photos": photos });
        if !notes.is_empty() {
            reply["notes"] = json!(notes);
        }
        Ok(vec![text(reply.to_string())])
    }

    /// The folder's lock for one writing command. Refused, with nothing
    /// changed, while LightPhotos has the folder open or another command
    /// is writing it. A volume without locks is written without one.
    fn lock(&self) -> Result<Option<sidecar::FolderLock>, String> {
        use sidecar::{FolderLock, Holder, LockError, Writer, LOCK_WAIT};
        match FolderLock::acquire(&self.root, Writer::Mcp, LOCK_WAIT) {
            Ok(lock) => Ok(Some(lock)),
            Err(
                e @ LockError::Busy(Some(Holder {
                    writer: Writer::Mcp,
                    ..
                })),
            ) => Err(format!("{e}; nothing was changed. Try again.")),
            Err(e @ LockError::Busy(_)) => Err(format!(
                "{e}, so headless lightphotos-mcp can't change it; nothing was changed. \
                 Send the command through the app instead: start it with \
                 `lightphotos --control <port> {}` and run `lightphotos-mcp --attach <port>`. \
                 Reading (get_state, render) still works here.",
                self.root.display()
            )),
            Err(e @ (LockError::Unavailable(_) | LockError::Unsupported(_))) => {
                eprintln!("lightphotos-mcp: {e}; saving without a lock");
                Ok(None)
            }
        }
    }

    fn state(&self) -> Result<Value, String> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .map_err(|e| format!("cannot list {}: {e}", self.root.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file() && image_decode::is_image_extension(p))
            .collect();
        paths.sort();
        let total = paths.len();
        let photos: Vec<Value> = paths
            .iter()
            .take(MAX_LISTED)
            .map(|p| match sidecar::read_record(p) {
                Ok(rec) => entry(p, &rec),
                Err(e) => json!({ "name": file_name(p), "sidecar_error": e }),
            })
            .collect();
        let mut state = json!({
            "folder": self.root,
            "photos": photos,
            "total": total,
            "truncated": total > MAX_LISTED,
        });
        // A folder another program holds refuses every writing command.
        if let Some(held) = sidecar::probe(&self.root) {
            state["read_only"] = json!(format!(
                "{held}; writing commands are refused here. Use lightphotos-mcp --attach to \
                 send them through the app."
            ));
        }
        Ok(state)
    }

    fn render(&self, name: &Path, max: u32) -> ToolResult {
        let path = self.photo(name)?;
        let rec = self.record(&path)?;
        let output = Output {
            max_px: max,
            ..Output::FULL
        };
        let masks = rec
            .mask_set(&path)
            .map_err(|e| format!("cannot render {e}"))?;
        let jpeg = export::export_file(
            &path,
            &rec.adjustments,
            &rec.touchups,
            &masks,
            rec.rotation,
            &output,
        )
        .map_err(|e| format!("cannot render {}: {e}", file_name(&path)))?;
        Ok(vec![
            image(lightphotos::base64::encode(&jpeg), "image/jpeg"),
            text(format!(
                "{} with its saved edits, at most {max} px on the long edge.",
                file_name(&path)
            )),
        ])
    }

    /// The photo `name` means: a file name in the folder, or an absolute
    /// path whose folder is this one. Anything else is refused, and so is a
    /// symlink to a file outside the folder, so a tool call cannot read or
    /// write outside the folder.
    fn photo(&self, name: &Path) -> Result<PathBuf, String> {
        let refuse = || {
            format!(
                "no photo `{}` in {}. Name a photo by its file name, as get_state lists it",
                name.display(),
                self.root.display()
            )
        };
        let file = if name.is_absolute() {
            let parent = name.parent().and_then(|p| p.canonicalize().ok());
            if parent.as_deref() != Some(self.root.as_path()) {
                return Err(refuse());
            }
            name.file_name().ok_or_else(refuse)?
        } else {
            let mut parts = name.components();
            match (parts.next(), parts.next()) {
                (Some(Component::Normal(file)), None) => file,
                _ => return Err(refuse()),
            }
        };
        let path = self.root.join(file);
        let inside = path
            .canonicalize()
            .is_ok_and(|real| real.starts_with(&self.root));
        if inside && path.is_file() && image_decode::is_image_extension(&path) {
            Ok(path)
        } else {
            Err(refuse())
        }
    }

    fn record(&self, path: &Path) -> Result<ImageRecord, String> {
        sidecar::read_record(path).map_err(|e| {
            format!(
                "cannot read the saved edits of {}: {e}; nothing was changed",
                file_name(path)
            )
        })
    }
}

/// Runs, inline, the detection each of `rec`'s detected masks still needs,
/// and records it, so the sidecar written next is complete. Headless there
/// is no worker to hand it to. Returns a note per target this build can't
/// detect or failed to, so the caller knows the mask renders as nothing.
fn detect_for(path: &Path, rec: &mut ImageRecord) -> Vec<String> {
    use lightphotos::develop::DetectTarget;
    use lightphotos::scoring::detect;
    let mut notes = Vec::new();
    for target in DetectTarget::ALL {
        if rec.detections.get(target).is_some()
            || !rec.masks.iter().any(|m| m.target() == Some(target))
        {
            continue;
        }
        let what = format!("{target:?}").to_lowercase();
        if !detect::available(target) {
            notes.push(format!(
                "{}: its {what} mask renders as nothing until LightPhotos on a Mac opens this folder",
                file_name(path)
            ));
            continue;
        }
        match detect::run(path, target) {
            Ok(coverage) => rec.detections.set(target, Some(coverage.id())),
            Err(e) => notes.push(format!(
                "{}: {what} detection failed, so the mask renders as nothing: {e}",
                file_name(path)
            )),
        }
    }
    notes
}

/// What `get_state` and `run_command` report for one photo.
fn entry(path: &Path, rec: &ImageRecord) -> Value {
    json!({
        "name": file_name(path),
        "rating": rec.rating,
        "flag": rec.flag,
        "label": rec.label,
        "edited": rec.edited.is_some(),
    })
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The photo's size as shown, which a straightened crop needs.
fn display_size(path: &Path) -> Result<(f32, f32), CommandError> {
    image_decode::read_metadata(path)
        .source_size
        .filter(|&(w, h)| w > 0 && h > 0)
        .map(|(w, h)| (w as f32, h as f32))
        .ok_or_else(|| CommandError::Failed {
            reason: format!(
                "cannot read the size of {}, which a straightened crop needs",
                file_name(path)
            ),
        })
}

/// A command error as a tool error that says what to do next.
fn explain(e: CommandError) -> String {
    match e {
        CommandError::NeedsApp => {
            let headless: Vec<&str> = Command::examples()
                .iter()
                .filter(|c| c.about().1)
                .map(Command::id)
                .collect();
            format!(
                "this command needs the running app. Headless, these run: {}. For the rest, \
                 start the app with `lightphotos --control <port> <folder>` and run \
                 lightphotos-mcp with --attach <port>.",
                headless.join(", ")
            )
        }
        CommandError::UnknownCommand { id } => {
            format!("no command `{id}`. Call list_commands to see every id.")
        }
        e => e.to_string(),
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use lightphotos::develop::{DetectTarget, Mask};

    /// Whether Vision failed because this machine can't run a model at all,
    /// as on GitHub's macOS runners: VMs with no GPU or Neural Engine.
    fn no_model_here(e: &lightphotos::scoring::detect::DetectError) -> bool {
        let why = e.to_string();
        why.contains("Could not create inference context") || why.contains("Unspecified error")
    }

    fn folder_with_photo(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lp-mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("a.jpg");
        lightphotos::decode::image_encode::encode_jpeg(
            &photo,
            32,
            24,
            &vec![90u8; 32 * 24 * 4],
            lightphotos::decode::image_encode::JpegQuality::Export,
        )
        .unwrap();
        (dir, photo)
    }

    /// A sidecar naming a coverage file that isn't there must fail the
    /// render by name, not hand back a silently unmasked photo.
    #[test]
    fn a_render_whose_coverage_file_is_missing_fails_naming_the_photo() {
        let (dir, photo) = folder_with_photo("missing-coverage");
        let mut rec = ImageRecord::default();
        rec.masks.set(vec![Mask::detected(DetectTarget::Subject)]);
        rec.detections.subject = "0123456789abcdef".parse().ok();
        sidecar::write_record(&photo, &rec).unwrap();
        let folder = Folder::open(&dir).unwrap();
        let err = folder.render(Path::new("a.jpg"), 64).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(err.contains("a.jpg") && err.contains("missing"), "{err}");
    }

    /// Headless, a detected mask's detection runs inline where this build
    /// can, and says so where it can't.
    #[test]
    fn detect_for_records_what_it_can_and_notes_the_rest() {
        let (dir, photo) = folder_with_photo("detect-for");
        if let Err(e) = lightphotos::scoring::detect::run(&photo, DetectTarget::Subject) {
            if no_model_here(&e) {
                eprintln!("skipping: Vision cannot run a model here ({e})");
                std::fs::remove_dir_all(&dir).unwrap();
                return;
            }
        }
        let mut rec = ImageRecord::default();
        rec.masks.set(vec![
            Mask::detected(DetectTarget::Subject),
            Mask::detected(DetectTarget::Sky),
        ]);
        let notes = detect_for(&photo, &mut rec);
        let subject_detected = rec.detections.subject.is_some();
        let subject_file = rec
            .detections
            .subject
            .is_some_and(|id| lightphotos::develop::coverage::read(&photo, id).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(subject_detected, cfg!(target_os = "macos"));
        assert_eq!(
            subject_file,
            cfg!(target_os = "macos"),
            "the file exists before the sidecar"
        );
        assert!(
            rec.detections.sky.is_some(),
            "sky runs on every native build"
        );
        if cfg!(target_os = "macos") {
            assert!(notes.is_empty(), "{notes:?}");
        } else {
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains("subject"), "{notes:?}");
        }
    }

    #[test]
    fn a_symlink_to_a_photo_outside_the_folder_is_refused() {
        let base = std::env::temp_dir().join(format!("lp-mcp-symlink-{}", std::process::id()));
        let (inside, outside) = (base.join("folder"), base.join("elsewhere"));
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.jpg"), b"not yours").unwrap();
        std::fs::write(inside.join("own.jpg"), b"yours").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), inside.join("link.jpg")).unwrap();
        std::os::unix::fs::symlink(inside.join("own.jpg"), inside.join("alias.jpg")).unwrap();
        let folder = Folder::open(&inside).unwrap();

        let leak = folder.photo(Path::new("link.jpg"));
        let absolute = folder.photo(&folder.root().join("link.jpg"));
        let alias = folder.photo(Path::new("alias.jpg"));
        std::fs::remove_dir_all(&base).unwrap();

        assert!(leak.is_err(), "followed a symlink out: {leak:?}");
        assert!(absolute.is_err(), "followed a symlink out: {absolute:?}");
        assert!(
            alias.is_ok(),
            "a symlink inside the folder is fine: {alias:?}"
        );
    }
}
