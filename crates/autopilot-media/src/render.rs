//! Render orchestration: plan -> Piper TTS -> ffmpeg assembly.
//!
//! GPU policy: this module never touches the GPU. Piper is CPU-only, and local
//! image generation (ComfyUI) must go through the `hl-infer` lease gateway — see
//! `mnemocine/guides/gpu-centralization.md`. The renderer only consumes still
//! images that already exist on disk, resolved through the [`ImageSource`] seam.
//! A GPU-backed implementation of that trait is deliberately out of scope here.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::assembly::{
    AssemblySegment, AssemblySpec, DEFAULT_FFPROBE_BINARY, Motion, SUBTITLE_FILE,
    build_assembly_command,
};
use crate::plan::{Beat, VideoPlan};
use crate::tts::{BuiltCommand, PiperConfig, build_tts_command};

/// Words-per-second assumption used to estimate beat length in `dry_run`.
pub const DRY_RUN_SECONDS_PER_WORD: f64 = 0.4;
/// Floor for the `dry_run` duration estimate, in seconds.
pub const MIN_DRY_RUN_SECONDS: f64 = 1.5;
/// Default timeout for one Piper synthesis.
pub const DEFAULT_TTS_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for the ffmpeg assembly.
pub const DEFAULT_FFMPEG_TIMEOUT: Duration = Duration::from_secs(900);

/// Resolves the still image a beat is spoken over.
///
/// **This is the seam for local image generation.** A GPU-backed implementation
/// would request an `image_gen` lease from the `hl-infer` gateway, generate the
/// frame and return its path. The renderer ships no GPU code on purpose: it
/// consumes existing files and validates them before running ffmpeg.
pub trait ImageSource: Send + Sync {
    /// Path to the still image for `beat`, relative to or inside `work_dir`.
    fn image_for(&self, beat: &Beat, work_dir: &Path) -> Result<PathBuf>;
}

/// Placeholder source that derives deterministic paths without touching disk.
///
/// Used by `dry_run` and by callers that only want the planned layout.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaceholderImageSource;

impl ImageSource for PlaceholderImageSource {
    fn image_for(&self, beat: &Beat, work_dir: &Path) -> Result<PathBuf> {
        Ok(work_dir.join(format!("scene-{:02}.png", beat.scene_index)))
    }
}

/// Source backed by images that already exist on disk, one per scene index.
#[derive(Debug, Clone)]
pub struct SceneImageFiles {
    paths: Vec<PathBuf>,
}

impl SceneImageFiles {
    /// Build the source from scene-indexed paths.
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl ImageSource for SceneImageFiles {
    fn image_for(&self, beat: &Beat, _work_dir: &Path) -> Result<PathBuf> {
        self.paths
            .get(beat.scene_index)
            .cloned()
            .ok_or_else(|| anyhow!("no image provided for scene {}", beat.scene_index))
    }
}

/// Source that resolves `<dir>/<scene_index>.png`.
#[derive(Debug, Clone)]
pub struct SceneImageDir {
    dir: PathBuf,
}

impl SceneImageDir {
    /// Build the source for a directory of scene images.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl ImageSource for SceneImageDir {
    fn image_for(&self, beat: &Beat, _work_dir: &Path) -> Result<PathBuf> {
        Ok(self.dir.join(format!("{}.png", beat.scene_index)))
    }
}

/// Options controlling one render.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub work_dir: PathBuf,
    pub tts: PiperConfig,
    pub motion: Motion,
    /// When `true`, nothing is executed and nothing is written.
    pub dry_run: bool,
    pub tts_timeout: Duration,
    pub ffmpeg_timeout: Duration,
}

impl RenderOptions {
    /// Options with the default timeouts.
    pub fn new(work_dir: PathBuf, tts: PiperConfig, motion: Motion, dry_run: bool) -> Self {
        Self {
            work_dir,
            tts,
            motion,
            dry_run,
            tts_timeout: DEFAULT_TTS_TIMEOUT,
            ffmpeg_timeout: DEFAULT_FFMPEG_TIMEOUT,
        }
    }
}

/// One command that was run (or would be run, in `dry_run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCommand {
    pub label: String,
    pub command: BuiltCommand,
}

/// The outcome of a render.
#[derive(Debug, Clone)]
pub struct RenderOutput {
    pub video: PathBuf,
    pub commands: Vec<RenderedCommand>,
    pub segments: Vec<AssemblySegment>,
    pub dry_run: bool,
}

/// Run the TTS then ffmpeg steps for a plan.
///
/// In `dry_run` no process is spawned and no file is written: the returned
/// commands are exactly what a real run would execute.
pub async fn render(
    plan: &VideoPlan,
    images: &dyn ImageSource,
    options: &RenderOptions,
) -> Result<RenderOutput> {
    plan.validate()?;
    let beats = plan.beats();
    if beats.is_empty() {
        bail!("the plan produced no beats to render");
    }

    if !options.dry_run {
        tokio::fs::create_dir_all(&options.work_dir)
            .await
            .with_context(|| {
                format!(
                    "creating the working directory {}",
                    options.work_dir.display()
                )
            })?;
    }

    let mut commands: Vec<RenderedCommand> = Vec::with_capacity(beats.len() + 1);
    let mut segments: Vec<AssemblySegment> = Vec::with_capacity(beats.len());

    for (index, beat) in beats.iter().enumerate() {
        let image = images.image_for(beat, &options.work_dir)?;
        let audio = options.work_dir.join(format!("beat-{index:02}.wav"));
        let tts = build_tts_command(&options.tts, &beat.narration, &audio)?;

        let duration_secs = if options.dry_run {
            estimate_speech_seconds(&beat.narration)
        } else {
            run_command(
                &tts,
                options.tts_timeout,
                &format!("piper beat {index}"),
                &options.work_dir,
            )
            .await?;
            probe_audio_seconds(DEFAULT_FFPROBE_BINARY, &audio).await?
        };

        if !options.dry_run && !image.is_file() {
            bail!(
                "no still image at {} for scene {}",
                image.display(),
                beat.scene_index
            );
        }

        commands.push(RenderedCommand {
            label: format!("tts beat {index}"),
            command: tts,
        });
        segments.push(AssemblySegment {
            image,
            audio,
            duration_secs,
        });
    }

    let video = options.work_dir.join("short.mp4");
    let subtitles = if options.dry_run {
        Some(options.work_dir.join(SUBTITLE_FILE))
    } else {
        let srt = build_srt(&beats, &segments);
        let path = options.work_dir.join(SUBTITLE_FILE);
        tokio::fs::write(&path, srt)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        Some(path)
    };

    let mut spec = AssemblySpec::vertical(segments.clone(), video.clone());
    spec.subtitles = subtitles;
    spec.motion = options.motion;
    let ffmpeg = build_assembly_command(&spec)?;

    if !options.dry_run {
        run_command(
            &ffmpeg,
            options.ffmpeg_timeout,
            "ffmpeg assembly",
            &options.work_dir,
        )
        .await?;
    }
    commands.push(RenderedCommand {
        label: "ffmpeg assembly".to_string(),
        command: ffmpeg,
    });

    Ok(RenderOutput {
        video,
        commands,
        segments,
        dry_run: options.dry_run,
    })
}

/// Estimate how long a narration takes to speak (used only in `dry_run`).
pub fn estimate_speech_seconds(narration: &str) -> f64 {
    let words = narration.split_whitespace().count().max(1) as f64;
    (words * DRY_RUN_SECONDS_PER_WORD).max(MIN_DRY_RUN_SECONDS)
}

/// Build an `.srt` file from the beats and their measured durations.
pub fn build_srt(beats: &[Beat], segments: &[AssemblySegment]) -> String {
    let mut out = String::new();
    let mut start = 0.0f64;
    for (index, (beat, segment)) in beats.iter().zip(segments).enumerate() {
        let end = start + segment.duration_secs;
        let _ = write!(
            out,
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_timestamp(start),
            srt_timestamp(end),
            beat.narration
        );
        start = end;
    }
    out
}

/// Read the duration of a media file with `ffprobe`, in seconds.
pub async fn probe_audio_seconds(ffprobe: &str, audio: &Path) -> Result<f64> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(audio)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {ffprobe}"))?;

    if !output.status.success() {
        bail!("{ffprobe} failed on {}", audio.display());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let value = text.trim().lines().next().unwrap_or_default().trim();
    value.parse::<f64>().with_context(|| {
        format!(
            "invalid duration `{value}` reported for {}",
            audio.display()
        )
    })
}

/// Spawn a built command, feed its stdin and fail on a non-zero exit.
async fn run_command(
    command: &BuiltCommand,
    timeout: Duration,
    label: &str,
    work_dir: &Path,
) -> Result<()> {
    let stdin = if command.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .current_dir(work_dir)
        .stdin(stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {label} ({})", command.program))?;

    if let Some(input) = &command.stdin
        && let Some(mut handle) = child.stdin.take()
    {
        handle
            .write_all(input.as_bytes())
            .await
            .with_context(|| format!("writing stdin for {label}"))?;
        let _ = handle.shutdown().await;
    }

    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .with_context(|| format!("{label} timed out after {timeout:?}"))?
        .with_context(|| format!("waiting for {label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{label} exited with {}: {}", output.status, stderr.trim());
    }
    Ok(())
}

/// Format a timestamp as `HH:MM:SS,mmm` (SubRip).
fn srt_timestamp(seconds: f64) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{hours:02}:{minutes:02}:{secs:02},{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Scene;

    fn plan() -> VideoPlan {
        VideoPlan {
            hook: "Gancho falado.".into(),
            scenes: vec![
                Scene {
                    narration: "Primeira cena.".into(),
                    image_prompt: "p0".into(),
                },
                Scene {
                    narration: "Segunda cena, um pouco mais longa para o teste.".into(),
                    image_prompt: "p1".into(),
                },
            ],
            call_to_action: "Siga!".into(),
        }
    }

    fn options(work_dir: PathBuf) -> RenderOptions {
        RenderOptions::new(
            work_dir,
            PiperConfig::new("/voices/test.onnx"),
            Motion::Static,
            true,
        )
    }

    #[tokio::test]
    async fn dry_run_returns_commands_without_touching_disk() {
        let work_dir = std::env::temp_dir().join(format!(
            "autopilot-media-dry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));

        let output = render(&plan(), &PlaceholderImageSource, &options(work_dir.clone()))
            .await
            .expect("dry run succeeds");

        assert!(output.dry_run);
        assert_eq!(output.commands.len(), 5, "4 beats + 1 ffmpeg");
        assert_eq!(output.segments.len(), 4);
        assert!(!work_dir.exists(), "dry_run must not create the work dir");
        assert_eq!(output.video, work_dir.join("short.mp4"));
    }

    #[test]
    fn estimates_speech_seconds_with_a_floor() {
        assert_eq!(estimate_speech_seconds("uma"), MIN_DRY_RUN_SECONDS);
        assert!(estimate_speech_seconds("um dois três quatro cinco seis sete oito") > 3.0);
    }

    #[test]
    fn builds_a_sequential_srt() {
        let beats = plan().beats();
        let segments = vec![
            AssemblySegment {
                image: PathBuf::from("a.png"),
                audio: PathBuf::from("a.wav"),
                duration_secs: 1.5,
            },
            AssemblySegment {
                image: PathBuf::from("b.png"),
                audio: PathBuf::from("b.wav"),
                duration_secs: 2.25,
            },
        ];
        let srt = build_srt(&beats[..2], &segments);
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,500\n"));
        assert!(srt.contains("00:00:01,500 --> 00:00:03,750"));
        assert!(srt.ends_with("\n\n"));
    }

    #[test]
    fn scene_image_files_reports_a_missing_scene() {
        let source = SceneImageFiles::new(vec![PathBuf::from("only.png")]);
        let beat = &plan().beats()[2];
        assert!(source.image_for(beat, Path::new(".")).is_err());
    }

    #[test]
    fn scene_image_dir_uses_the_scene_index() {
        let source = SceneImageDir::new(PathBuf::from("/imgs"));
        let beat = &plan().beats()[3];
        assert_eq!(
            source.image_for(beat, Path::new(".")).expect("path"),
            PathBuf::from("/imgs/1.png")
        );
    }
}
