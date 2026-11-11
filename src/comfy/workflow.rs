// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fills LightPhotos' inputs into an API-format ComfyUI workflow: a JSON
//! object of node id to `{class_type, inputs, _meta}`, the form ComfyUI's
//! "Export (API)" writes and `POST /prompt` takes. A workflow that breaks the
//! rules below gets an error naming what to change in it.

use serde_json::{Map, Value};

/// The `_meta.title` of the text-encode node that gets the user's prompt.
pub const PROMPT_TITLE: &str = "LightPhotos Prompt";

/// A workflow ready to queue, and the node whose images are the result.
#[derive(Debug)]
pub struct Injected {
    pub prompt: Value,
    pub output_node: String,
}

/// A model file a workflow's loader node names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRef {
    pub loader: &'static str,
    pub input: &'static str,
    /// The folder under ComfyUI's `models/` the loader lists.
    pub folder: &'static str,
    pub file: String,
}

/// Loader class, the input naming its file, and the models folder it lists.
/// `DualCLIPLoader` names two files, so it has two rows.
const LOADERS: &[(&str, &str, &str)] = &[
    ("CheckpointLoaderSimple", "ckpt_name", "checkpoints"),
    ("UNETLoader", "unet_name", "diffusion_models"),
    ("VAELoader", "vae_name", "vae"),
    ("CLIPLoader", "clip_name", "text_encoders"),
    ("DualCLIPLoader", "clip_name1", "text_encoders"),
    ("DualCLIPLoader", "clip_name2", "text_encoders"),
];

/// The loader classes [`model_files`] reads, each once.
pub fn loader_classes() -> Vec<&'static str> {
    let mut classes: Vec<&str> = LOADERS.iter().map(|(class, ..)| *class).collect();
    classes.dedup();
    classes
}

/// Parses `json` as an API-format workflow.
pub fn parse(json: &str) -> Result<Map<String, Value>, String> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| format!("the workflow isn't valid JSON: {e}"))?;
    let Value::Object(nodes) = value else {
        return Err(
            "the workflow isn't in API format. In ComfyUI, use Workflow ▸ Export (API).".into(),
        );
    };
    // The UI format has a top-level `nodes` array instead of node ids.
    if nodes.contains_key("nodes") && nodes.contains_key("links") {
        return Err(
            "the workflow isn't in API format. In ComfyUI, use Workflow ▸ Export (API).".into(),
        );
    }
    Ok(nodes)
}

fn class_type(node: &Value) -> Option<&str> {
    node.get("class_type").and_then(Value::as_str)
}

fn title(node: &Value) -> Option<&str> {
    node.get("_meta")
        .and_then(|m| m.get("title"))
        .and_then(Value::as_str)
}

/// The node's `inputs` object, created if the node has none.
fn inputs_mut<'a>(id: &str, node: &'a mut Value) -> Result<&'a mut Map<String, Value>, String> {
    let Value::Object(node) = node else {
        return Err(format!("workflow node {id} isn't an object"));
    };
    match node
        .entry("inputs")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(inputs) => Ok(inputs),
        _ => Err(format!(
            "workflow node {id} has inputs that aren't an object"
        )),
    }
}

/// Node ids in a stable order: numeric ids by value, then the rest by text.
fn sorted_ids(nodes: &Map<String, Value>) -> Vec<String> {
    let mut ids: Vec<String> = nodes.keys().cloned().collect();
    ids.sort_by(|a, b| match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    });
    ids
}

/// Sets the uploaded `image`, the `prompt` text and the `seed` in a copy of
/// `workflow`, and finds its output node.
pub fn inject(
    workflow: &Map<String, Value>,
    image: &str,
    prompt: &str,
    seed: u64,
) -> Result<Injected, String> {
    let mut nodes = workflow.clone();
    let ids = sorted_ids(&nodes);

    let load_images: Vec<&String> = ids
        .iter()
        .filter(|id| nodes.get(*id).and_then(class_type) == Some("LoadImage"))
        .collect();
    let [load_image] = load_images.as_slice() else {
        return Err(format!(
            "the workflow needs exactly one LoadImage node for the photo, and it has {}",
            load_images.len()
        ));
    };

    let prompt_nodes: Vec<&String> = ids
        .iter()
        .filter(|id| nodes.get(*id).and_then(title) == Some(PROMPT_TITLE))
        .collect();
    let prompt_node = match prompt_nodes.as_slice() {
        [] if prompt.is_empty() => None,
        [] => {
            return Err(format!(
                "the workflow has no text node titled \"{PROMPT_TITLE}\" to take the prompt"
            ));
        }
        [one] => Some(*one),
        many => {
            return Err(format!(
                "the workflow has {} nodes titled \"{PROMPT_TITLE}\" and needs one",
                many.len()
            ));
        }
    };

    let output_node = ids
        .iter()
        .find(|id| nodes.get(*id).and_then(class_type) == Some("SaveImage"))
        .cloned()
        .ok_or("the workflow has no SaveImage node for the result")?;

    let (load_image, prompt_node) = ((*load_image).clone(), prompt_node.cloned());
    if let Some(node) = nodes.get_mut(&load_image) {
        inputs_mut(&load_image, node)?.insert("image".into(), image.into());
    }
    if let Some((id, node)) = prompt_node.and_then(|id| nodes.get_mut(&id).map(|n| (id, n))) {
        inputs_mut(&id, node)?.insert("text".into(), prompt.into());
    }
    for node in nodes.values_mut() {
        // Only a literal number is a seed to set. An array is a link from
        // another node's output, which is left alone.
        let Some(Value::Object(inputs)) = node.get_mut("inputs") else {
            continue;
        };
        for name in ["seed", "noise_seed"] {
            if let Some(value) = inputs.get_mut(name).filter(|v| v.is_u64()) {
                *value = seed.into();
            }
        }
    }

    Ok(Injected {
        prompt: Value::Object(nodes),
        output_node,
    })
}

/// Every model file the workflow's loader nodes name, in node order.
pub fn model_files(workflow: &Map<String, Value>) -> Vec<ModelRef> {
    let mut found = Vec::new();
    for id in sorted_ids(workflow) {
        let Some(node) = workflow.get(&id) else {
            continue;
        };
        let Some(class) = class_type(node) else {
            continue;
        };
        for (loader, input, folder) in LOADERS.iter().filter(|(c, ..)| *c == class) {
            if let Some(file) = node
                .get("inputs")
                .and_then(|i| i.get(*input))
                .and_then(Value::as_str)
            {
                found.push(ModelRef {
                    loader,
                    input,
                    folder,
                    file: file.to_string(),
                });
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(json: &str) -> Map<String, Value> {
        parse(json).unwrap()
    }

    const MINIMAL: &str = r#"{
        "10": {"class_type": "SaveImage", "inputs": {"images": ["8", 0]}},
        "2": {"class_type": "SaveImage", "inputs": {"images": ["8", 0]}},
        "1": {"class_type": "LoadImage", "inputs": {"image": "old.png"}},
        "5": {"class_type": "CLIPTextEncode", "inputs": {"text": "", "clip": ["4", 0]},
              "_meta": {"title": "LightPhotos Prompt"}},
        "6": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry", "clip": ["4", 0]}},
        "7": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 20, "model": ["3", 0]}},
        "9": {"class_type": "SamplerCustom", "inputs": {"noise_seed": 2}},
        "11": {"class_type": "KSampler", "inputs": {"seed": ["12", 0]}}
    }"#;

    #[test]
    fn inject_sets_image_prompt_and_every_literal_seed() {
        let out = inject(&nodes(MINIMAL), "lp.png", "a red barn", 42).unwrap();
        let p = &out.prompt;
        assert_eq!(p["1"]["inputs"]["image"], "lp.png");
        assert_eq!(p["5"]["inputs"]["text"], "a red barn");
        assert_eq!(
            p["6"]["inputs"]["text"], "blurry",
            "the negative is untouched"
        );
        assert_eq!(p["7"]["inputs"]["seed"], 42);
        assert_eq!(
            p["7"]["inputs"]["steps"], 20,
            "other integers are untouched"
        );
        assert_eq!(p["9"]["inputs"]["noise_seed"], 42);
        assert_eq!(p["11"]["inputs"]["seed"], serde_json::json!(["12", 0]));
        assert_eq!(
            out.output_node, "2",
            "first SaveImage by numeric id, not text"
        );
    }

    #[test]
    fn load_image_count_must_be_one() {
        let none = r#"{"1": {"class_type": "SaveImage", "inputs": {}}}"#;
        let err = inject(&nodes(none), "x", "", 0).unwrap_err();
        assert!(err.contains("has 0"), "{err}");
        let two = r#"{"1": {"class_type": "LoadImage", "inputs": {}},
                      "2": {"class_type": "LoadImage", "inputs": {}},
                      "3": {"class_type": "SaveImage", "inputs": {}}}"#;
        let err = inject(&nodes(two), "x", "", 0).unwrap_err();
        assert!(err.contains("has 2"), "{err}");
    }

    #[test]
    fn prompt_node_is_optional_only_for_an_empty_prompt() {
        let json = r#"{"1": {"class_type": "LoadImage"},
                       "2": {"class_type": "SaveImage", "inputs": {}}}"#;
        let out = inject(&nodes(json), "x.png", "", 0).unwrap();
        assert_eq!(
            out.prompt["1"]["inputs"]["image"], "x.png",
            "inputs created"
        );
        let err = inject(&nodes(json), "x.png", "sky", 0).unwrap_err();
        assert!(err.contains(PROMPT_TITLE), "{err}");
    }

    #[test]
    fn missing_save_image_is_an_error() {
        let json = r#"{"1": {"class_type": "LoadImage", "inputs": {}}}"#;
        let err = inject(&nodes(json), "x", "", 0).unwrap_err();
        assert!(err.contains("SaveImage"), "{err}");
    }

    #[test]
    fn malformed_workflows_are_errors_not_panics() {
        for bad in ["", "[]", "42", "{", r#"{"nodes": [], "links": []}"#] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let odd = r#"{"1": {"class_type": "LoadImage", "inputs": 7},
                      "2": {"class_type": "SaveImage"}, "3": "node", "4": null,
                      "5": {"class_type": 9, "inputs": {"seed": -1}}}"#;
        let err = inject(&nodes(odd), "x", "", 0).unwrap_err();
        assert!(err.contains("inputs"), "{err}");
        assert!(model_files(&nodes(odd)).is_empty());
    }

    #[test]
    fn model_files_reads_each_loader_input() {
        let json = r#"{
            "1": {"class_type": "UNETLoader", "inputs": {"unet_name": "u.safetensors"}},
            "2": {"class_type": "DualCLIPLoader",
                  "inputs": {"clip_name1": "l.safetensors", "clip_name2": "g.safetensors"}},
            "3": {"class_type": "VAELoader", "inputs": {"vae_name": "v.safetensors"}},
            "4": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": ["9", 0]}}
        }"#;
        let files: Vec<(&str, String)> = model_files(&nodes(json))
            .into_iter()
            .map(|m| (m.folder, m.file))
            .collect();
        assert_eq!(
            files,
            [
                ("diffusion_models", "u.safetensors".to_string()),
                ("text_encoders", "l.safetensors".to_string()),
                ("text_encoders", "g.safetensors".to_string()),
                ("vae", "v.safetensors".to_string()),
            ]
        );
    }
}
