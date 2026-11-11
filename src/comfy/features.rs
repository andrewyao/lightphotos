// SPDX-License-Identifier: MIT OR Apache-2.0

//! The AI features LightPhotos runs through ComfyUI. Each row names the
//! workflow it queues and the model files that workflow loads, so Settings can
//! offer to download them. LightPhotos ships no weights.

/// A model file a feature needs, and where to get it.
#[derive(Debug)]
pub struct ModelFile {
    /// The folder under ComfyUI's `models/`.
    pub folder: &'static str,
    /// The name it is saved under, which the workflow's loader names.
    pub file: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    /// Lowercase hex.
    pub sha256: &'static str,
}

#[derive(Debug)]
pub struct ComfyFeature {
    pub id: &'static str,
    pub name: &'static str,
    /// An API-format workflow. See [`super::workflow`].
    pub workflow: &'static str,
    pub models: &'static [ModelFile],
}

macro_rules! sdxl_inpainting {
    ($path:literal) => {
        concat!(
            "https://huggingface.co/diffusers/stable-diffusion-xl-1.0-inpainting-0.1/resolve/main",
            $path
        )
    };
}

/// Generative Fill uses SDXL Inpainting 0.1's fp16 weights straight from the
/// diffusers repo, which has no single-file checkpoint. Stock ComfyUI loads
/// each part as it is: `UNETLoader` converts a diffusers UNet with 9 input
/// channels into its SDXL inpainting model, `DualCLIPLoader` (type `sdxl`)
/// reads the two transformers text encoders, and `VAELoader` converts a
/// diffusers VAE. The files are renamed because every part's file has the
/// same name upstream.
pub const FEATURES: &[ComfyFeature] = &[ComfyFeature {
    id: "generative_fill",
    name: "Generative Fill",
    workflow: include_str!("../../assets/comfyui/generative_fill.json"),
    models: &[
        ModelFile {
            folder: "diffusion_models",
            file: "sdxl_inpainting_0.1_unet.fp16.safetensors",
            url: sdxl_inpainting!("/unet/diffusion_pytorch_model.fp16.safetensors"),
            bytes: 5_135_178_560,
            sha256: "6470840731e98cc16713ddf3ac7ee458c9fdbcb881a98c6727cd4a938f227d3f",
        },
        ModelFile {
            folder: "text_encoders",
            file: "sdxl_inpainting_0.1_clip_l.fp16.safetensors",
            url: sdxl_inpainting!("/text_encoder/model.fp16.safetensors"),
            bytes: 246_144_867,
            sha256: "fc83cf401d930147807e7c44021c164bcc5508c9d4cc0ff35f4e354685ca9cd0",
        },
        ModelFile {
            folder: "text_encoders",
            file: "sdxl_inpainting_0.1_clip_g.fp16.safetensors",
            url: sdxl_inpainting!("/text_encoder_2/model.fp16.safetensors"),
            bytes: 1_389_382_884,
            sha256: "a8622bd41f8d359df484fdf9de091edb9337a2fd747f0a5c9c320a62e24d1fd3",
        },
        ModelFile {
            folder: "vae",
            file: "sdxl_inpainting_0.1_vae.fp16.safetensors",
            url: sdxl_inpainting!("/vae/diffusion_pytorch_model.fp16.safetensors"),
            bytes: 167_335_338,
            sha256: "4ad62825e5c8b31eefb77355ba6693785619bf7d669d9b1a6fd9f19dec6d65b3",
        },
    ],
}];

#[cfg(test)]
mod tests {
    use super::super::workflow;
    use super::*;

    #[test]
    fn each_workflow_injects_and_names_exactly_its_models() {
        for feature in FEATURES {
            let nodes = workflow::parse(feature.workflow).unwrap();
            workflow::inject(&nodes, "x.png", "a prompt", 7)
                .unwrap_or_else(|e| panic!("{}: {e}", feature.id));
            let mut named: Vec<(&str, String)> = workflow::model_files(&nodes)
                .into_iter()
                .map(|m| (m.folder, m.file))
                .collect();
            let mut listed: Vec<(&str, String)> = feature
                .models
                .iter()
                .map(|m| (m.folder, m.file.to_string()))
                .collect();
            named.sort();
            listed.sort();
            assert_eq!(named, listed, "{}", feature.id);
        }
    }

    #[test]
    fn model_rows_are_well_formed() {
        for m in FEATURES.iter().flat_map(|f| f.models) {
            assert_eq!(m.sha256.len(), 64, "{}", m.file);
            assert!(m
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert!(m.url.starts_with("https://"), "{}", m.url);
            assert!(m.bytes > 0);
        }
    }
}
