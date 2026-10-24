//! The tool set and the mode that answers it.

use serde_json::{json, Map, Value};

use crate::{attached, headless};

/// Where the tools act: a running app, or a folder on disk.
pub enum Mode {
    Attached(attached::App),
    Headless(headless::Folder),
}

impl Mode {
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Mode, String> {
        let (mut folder, mut port) = (None, None);
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} takes a value"));
            match arg.as_str() {
                "--folder" => folder = Some(value()?),
                "--attach" => {
                    let v = value()?;
                    port = Some(
                        v.parse::<u16>()
                            .map_err(|_| format!("--attach takes a port, not `{v}`"))?,
                    );
                }
                "--token" => {
                    return Err(format!(
                        "pass the token in {}, not as a flag, which ps shows to every user",
                        attached::TOKEN_ENV
                    ))
                }
                _ => return Err(format!("unknown argument `{arg}`")),
            }
        }
        match (folder, port) {
            (Some(dir), None) => {
                let folder = headless::Folder::open(std::path::Path::new(&dir))?;
                eprintln!("[lightphotos-mcp] headless on {}", folder.root().display());
                Ok(Mode::Headless(folder))
            }
            (None, Some(port)) => {
                let token = std::env::var(attached::TOKEN_ENV)
                    .ok()
                    .filter(|t| !t.is_empty());
                if token.is_none() {
                    eprintln!("[lightphotos-mcp] no token: set {}", attached::TOKEN_ENV);
                }
                eprintln!("[lightphotos-mcp] attached to 127.0.0.1:{port}");
                Ok(Mode::Attached(attached::App::new(port, token)))
            }
            _ => Err("pass either --folder <dir> or --attach <port>".into()),
        }
    }
}

/// The tools. Each mode offers five of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    ListCommands,
    RunCommand,
    GetState,
    ListPhotos,
    Screenshot,
    Render,
}

impl Tool {
    const ALL: [Tool; 6] = [
        Tool::ListCommands,
        Tool::RunCommand,
        Tool::GetState,
        Tool::ListPhotos,
        Tool::Screenshot,
        Tool::Render,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::ListCommands => "list_commands",
            Tool::RunCommand => "run_command",
            Tool::GetState => "get_state",
            Tool::ListPhotos => "list_photos",
            Tool::Screenshot => "screenshot",
            Tool::Render => "render",
        }
    }

    pub fn parse(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// A tool's content items, or the text of a tool error that tells the agent
/// what to do next.
pub type ToolResult = Result<Vec<Value>, String>;

pub fn text(s: impl Into<String>) -> Value {
    json!({ "type": "text", "text": s.into() })
}

pub fn image(base64: String, mime: &str) -> Value {
    json!({ "type": "image", "data": base64, "mimeType": mime })
}

/// The JSON-RPC wire form of `run_command`'s arguments: `{id, params?}`.
pub fn command_request(args: &Map<String, Value>) -> Result<Value, String> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or("pass `id`, a command id from list_commands, such as photo.rate")?;
    let mut request = json!({ "id": id });
    if let Some(params) = args.get("params") {
        request["params"] = params.clone();
    }
    Ok(request)
}

impl Mode {
    /// The `tools/list` entries: the tools this mode answers.
    pub fn tools(&self) -> Vec<Value> {
        let attached = matches!(self, Mode::Attached(_));
        let object = |properties: Value, required: &[&str]| json!({ "type": "object", "properties": properties, "required": required });
        let photo = "A photo in the folder: its file name, such as IMG_0042.JPG, \
                     or its absolute path.";
        Tool::ALL
            .into_iter()
            .filter_map(|tool| {
                let (description, schema) = match tool {
                    Tool::ListCommands if attached => (
                        "Every command id with an example of its params, and every develop \
                         control with its range and the shown photo's value. Call this first.",
                        object(json!({}), &[]),
                    ),
                    Tool::ListCommands => (
                        "Every command id with an example of its params, and every develop \
                         control with its range. `headless: true` marks the commands this \
                         mode runs. Pass `photo` to see that photo's control values. Call \
                         this first.",
                        object(json!({ "photo": { "type": "string", "description": photo } }), &[]),
                    ),
                    Tool::RunCommand => (
                        if attached {
                            "Run one command in the app. Without `photos` in params it acts on \
                             the app's selection."
                        } else {
                            "Run one headless command on the photos named in params.photos, \
                             saving each photo's sidecar. Every photo is checked before any is \
                             written."
                        },
                        object(
                            json!({
                                "id": { "type": "string", "description": "A command id from list_commands, such as photo.rate." },
                                "params": { "type": "object", "description": "The command's params; list_commands shows an example of each." },
                            }),
                            &["id"],
                        ),
                    ),
                    Tool::GetState if attached => (
                        "The view, the selection, and the current photo's rating, flag and \
                         label. `wait: true` first waits up to 10 s for decodes, saves and \
                         animations to finish.",
                        object(json!({ "wait": { "type": "boolean", "default": false } }), &[]),
                    ),
                    Tool::GetState => (
                        "The folder's photos with each one's rating, flag, label, whether \
                         it has develop edits, and its 0-100 quality score once scored \
                         (`stale` once edited since). photo.score scores photos, on \
                         macOS only.",
                        object(json!({}), &[]),
                    ),
                    Tool::ListPhotos if attached => (
                        "Every photo the Grid shows, in its order, with its rating, flag, \
                         label, quality score and whether it has develop edits. Stops at 5000 \
                         photos with `truncated: true`.",
                        object(json!({}), &[]),
                    ),
                    Tool::ListPhotos => (
                        "The folder's photos, as get_state lists them.",
                        object(json!({}), &[]),
                    ),
                    Tool::Screenshot if attached => (
                        "A PNG of the app window. Waits for the app to settle first unless \
                         `wait` is false.",
                        object(json!({ "wait": { "type": "boolean", "default": true } }), &[]),
                    ),
                    Tool::Render if !attached => (
                        "A JPEG of a photo with its saved edits (develop, crop, rotation, \
                         touch-ups), as an export renders it, at most `max_size` px on its \
                         long edge.",
                        object(
                            json!({
                                "photo": { "type": "string", "description": photo },
                                "max_size": { "type": "integer", "minimum": headless::MIN_RENDER, "maximum": headless::MAX_RENDER, "default": headless::DEFAULT_RENDER },
                            }),
                            &["photo"],
                        ),
                    ),
                    Tool::Screenshot | Tool::Render => return None,
                };
                Some(json!({ "name": tool.name(), "description": description, "inputSchema": schema }))
            })
            .collect()
    }

    /// What `initialize` tells the agent about using the tools.
    pub fn instructions(&self) -> String {
        let common = format!(
            "{} You can open only the folder the user has open or a folder inside it, and \
             export only into those or the export form's own folder; an export keeps \
             location data only when the form does.",
            lightphotos::command::AGENT_NOTES
        );
        match self {
            Mode::Attached(_) => format!(
                "{common} You are attached to the running app: commands act on what it \
                 shows and selects, get_state reads it, list_photos lists the photos the \
                 Grid shows, and screenshot shows its window."
            ),
            Mode::Headless(folder) => format!(
                "{common} You work headless on the folder {}: get_state lists its photos, \
                 commands marked headless run on the photos you name in params.photos, and \
                 render shows a photo with its edits. While the LightPhotos app has this \
                 folder open, commands that write are refused and change nothing; get_state \
                 then reports read_only. Ask the user to attach lightphotos-mcp to the app \
                 (--attach) instead.",
                folder.root().display()
            ),
        }
    }

    pub fn call(&self, tool: Tool, args: &Map<String, Value>) -> ToolResult {
        match self {
            Mode::Attached(app) => app.call(tool, args),
            Mode::Headless(folder) => folder.call(tool, args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_modes_offer_list_photos() {
        let attached = Mode::Attached(attached::App::new(1, None));
        let dir = std::env::temp_dir().join(format!("lpmcp-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let headless = Mode::Headless(headless::Folder::open(&dir).unwrap());
        for mode in [&attached, &headless] {
            let names: Vec<Value> = mode.tools().iter().map(|t| t["name"].clone()).collect();
            assert_eq!(names.len(), 5, "{names:?}");
            assert!(names.contains(&json!("list_photos")), "{names:?}");
        }
        assert_eq!(Tool::parse("list_photos"), Some(Tool::ListPhotos));
        let listed = headless.call(Tool::ListPhotos, &Map::new()).unwrap();
        let reply: Value = serde_json::from_str(listed[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            (reply["total"].clone(), reply["truncated"].clone()),
            (json!(0), json!(false))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_token_is_not_taken_as_a_flag() {
        let args = ["--attach", "7777", "--token", "0123456789abcdef"].map(String::from);
        let err = Mode::from_args(args).err();
        assert!(
            err.as_deref()
                .is_some_and(|e| e.contains(attached::TOKEN_ENV)),
            "{err:?}"
        );
    }
}
