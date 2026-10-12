//! Short-video / image media module (pipeline phase 2).
//!
//! The module is deliberately split into a **pure** part (plan parsing, Piper
//! command construction, ffmpeg filtergraph construction — all unit-tested
//! without any external binary) and an **effectful** part ([`render`], which
//! spawns `piper` and `ffmpeg` through `tokio::process::Command`).
//!
//! GPU policy: nothing here touches the GPU. Piper is CPU-only by design, and
//! local image generation (ComfyUI) must go through the `hl-infer` lease
//! gateway — see `mnemocine/guides/gpu-centralization.md`. The renderer only
//! consumes still images that already exist on disk, behind the
//! [`render::ImageSource`] seam.

pub mod assembly;
pub mod json;
pub mod plan;
pub mod render;
pub mod tts;

pub use assembly::{
    AssemblySegment, AssemblySpec, DEFAULT_FFMPEG_BINARY, DEFAULT_FFPROBE_BINARY, DEFAULT_FPS,
    Motion, SUBTITLE_FILE, VERTICAL_HEIGHT, VERTICAL_WIDTH, build_assembly_command,
};
pub use json::extract_json;
pub use plan::{Beat, BeatRole, Scene, VideoPlan};
pub use render::{
    ImageSource, PlaceholderImageSource, RenderOptions, RenderOutput, RenderedCommand,
    SceneImageDir, SceneImageFiles, build_srt, estimate_speech_seconds, probe_audio_seconds,
    render,
};
pub use tts::{
    BuiltCommand, DEFAULT_PIPER_BINARY, PiperConfig, PiperProbe, build_tts_command,
    find_executable, probe_piper,
};
