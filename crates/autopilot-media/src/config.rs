//! Media configuration (`config/media.yaml`).
//!
//! The TTS binary and voice model are **data**, not constants. On the validated
//! host `/usr/bin/piper` belongs to `piper 0.8-5` = **libratbag** (a mouse tool),
//! so a hardcoded `piper` would silently run the wrong program; the config points
//! at the `piper-tts` install and its voice model instead. Every value has a safe
//! default, so the engine still runs text and cards with no media config at all.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::assembly::Motion;
use crate::tts::PiperConfig;

fn default_motion() -> Motion {
    Motion::Static
}

/// Settings for one slot's short video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaConfig {
    /// Local, CPU-only text-to-speech (Piper) settings.
    #[serde(default)]
    pub piper: PiperConfig,
    /// Still-image motion for each scene: `static` or `kenburns`.
    #[serde(default = "default_motion")]
    pub motion: Motion,
    /// Directory holding one PNG per scene, named `<scene_index>.png`.
    ///
    /// When absent the renderer falls back to the placeholder layout, which
    /// aborts a live render (there are no real stills yet — the GPU image source
    /// is the `hl-infer`-leased ComfyUI path, not implemented here).
    #[serde(default)]
    pub scene_images: Option<PathBuf>,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            piper: PiperConfig::default(),
            motion: default_motion(),
            scene_images: None,
        }
    }
}

impl MediaConfig {
    /// Load the configuration from a YAML file.
    pub async fn load(path: &Path) -> Result<Self> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("reading media config {}", path.display()))?;
        Self::parse_yaml(&raw)
    }

    /// Load the configuration, falling back to the defaults when the file is
    /// absent (a missing file never fails a run that does not need media).
    pub async fn load_or_default(path: &Path) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(raw) => Self::parse_yaml(&raw),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => {
                Err(err).with_context(|| format!("reading media config {}", path.display()))
            }
        }
    }

    /// Parse the configuration from a YAML string.
    pub fn parse_yaml(raw: &str) -> Result<Self> {
        serde_yaml_ng::from_str(raw).context("parsing media config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::DEFAULT_PIPER_BINARY;

    #[test]
    fn an_empty_config_uses_the_safe_defaults() {
        let config = MediaConfig::parse_yaml("{}").expect("empty config parses");
        assert_eq!(config.piper.binary, DEFAULT_PIPER_BINARY);
        assert!(config.piper.model.as_os_str().is_empty());
        assert_eq!(config.motion, Motion::Static);
        assert_eq!(config.scene_images, None);
    }

    #[test]
    fn the_config_points_at_piper_tts_and_a_voice() {
        // The shape the owner writes: the piper-tts binary and its `.onnx` voice.
        let yaml = "piper:\n  binary: /opt/piper-tts/bin/piper\n  \
                    model: /voices/pt_BR-faber-medium.onnx\n  length_scale: 1.1\n\
                    motion: kenburns\nscene_images: var/media/scenes\n";
        let config = MediaConfig::parse_yaml(yaml).expect("config parses");
        assert_eq!(config.piper.binary, "/opt/piper-tts/bin/piper");
        assert_eq!(
            config.piper.model,
            PathBuf::from("/voices/pt_BR-faber-medium.onnx")
        );
        assert_eq!(config.piper.length_scale, Some(1.1));
        assert_eq!(config.motion, Motion::KenBurns);
        assert_eq!(config.scene_images, Some(PathBuf::from("var/media/scenes")));
    }

    #[test]
    fn an_unknown_motion_is_rejected() {
        assert!(MediaConfig::parse_yaml("motion: wobble\n").is_err());
    }
}
