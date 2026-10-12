//! ffmpeg assembly command construction for a vertical 1080x1920 slideshow.
//!
//! The builder is pure: it validates the spec and returns the exact argv
//! (inputs, `-filter_complex`, mapping and encoder flags). It never spawns
//! ffmpeg — the effectful part lives in [`crate::render`].
//!
//! One still image is shown per scene for the length of that scene's narration
//! audio, then the per-scene clips are concatenated (`concat=n=N:v=1:a=1`) and
//! the subtitles are burned in with the `subtitles` filter (libass).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::tts::BuiltCommand;

/// Vertical output width (portrait, 9:16).
pub const VERTICAL_WIDTH: u32 = 1080;
/// Vertical output height (portrait, 9:16).
pub const VERTICAL_HEIGHT: u32 = 1920;
/// Default output frame rate.
pub const DEFAULT_FPS: u32 = 30;
/// Default ffmpeg binary name, resolved through `PATH`.
pub const DEFAULT_FFMPEG_BINARY: &str = "ffmpeg";
/// Default ffprobe binary name, resolved through `PATH`.
pub const DEFAULT_FFPROBE_BINARY: &str = "ffprobe";
/// Fixed subtitle file name inside the working directory.
pub const SUBTITLE_FILE: &str = "subtitles.srt";

/// ASS subtitle styling applied by the `subtitles` filter (`force_style`).
const SUBTITLE_STYLE: &str = "FontName=DejaVu Sans,FontSize=48,Bold=1,PrimaryColour=&H00FFFFFF,\
     OutlineColour=&H00000000,BorderStyle=1,Outline=3,Shadow=0,Alignment=2,MarginV=140";

/// Still-image motion for each scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Scale to fit and pad with black (safe on every ffmpeg build).
    Static,
    /// Slow Ken Burns zoom/pan via the `zoompan` filter.
    KenBurns,
}

impl Motion {
    /// Lowercase label used in CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Motion::Static => "static",
            Motion::KenBurns => "kenburns",
        }
    }
}

impl std::str::FromStr for Motion {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "static" => Ok(Motion::Static),
            "kenburns" | "ken-burns" => Ok(Motion::KenBurns),
            other => Err(format!(
                "unknown motion `{other}` (expected: static, kenburns)"
            )),
        }
    }
}

/// One scene segment: a still image shown for the length of its narration audio.
#[derive(Debug, Clone, PartialEq)]
pub struct AssemblySegment {
    pub image: PathBuf,
    pub audio: PathBuf,
    /// Duration of the narration audio, in seconds.
    pub duration_secs: f64,
}

/// Everything ffmpeg needs to assemble the final video.
#[derive(Debug, Clone, PartialEq)]
pub struct AssemblySpec {
    pub segments: Vec<AssemblySegment>,
    pub output: PathBuf,
    /// Optional `.srt` file burned into the video.
    pub subtitles: Option<PathBuf>,
    pub motion: Motion,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl AssemblySpec {
    /// A static vertical spec for the given segments and output path.
    pub fn vertical(segments: Vec<AssemblySegment>, output: PathBuf) -> Self {
        Self {
            segments,
            output,
            subtitles: None,
            motion: Motion::Static,
            width: VERTICAL_WIDTH,
            height: VERTICAL_HEIGHT,
            fps: DEFAULT_FPS,
        }
    }
}

/// Build the ffmpeg command that assembles the slideshow.
pub fn build_assembly_command(spec: &AssemblySpec) -> Result<BuiltCommand> {
    validate(spec)?;

    let mut args = vec![
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];

    // Interleaved inputs: image i is input 2i, audio i is input 2i+1.
    // `-framerate` pins the looped still to the output rate so that the Ken
    // Burns `zoompan` (and the plain `fps` filter) keep exact audio duration.
    for segment in &spec.segments {
        args.push("-framerate".to_string());
        args.push(spec.fps.to_string());
        args.push("-loop".to_string());
        args.push("1".to_string());
        args.push("-t".to_string());
        args.push(format!("{:.3}", segment.duration_secs));
        args.push("-i".to_string());
        args.push(segment.image.display().to_string());
        args.push("-i".to_string());
        args.push(segment.audio.display().to_string());
    }

    args.push("-filter_complex".to_string());
    args.push(build_filtergraph(spec));

    args.push("-map".to_string());
    args.push("[vout]".to_string());
    args.push("-map".to_string());
    args.push("[aout]".to_string());

    let fps = spec.fps.to_string();
    let tail = [
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-preset",
        "medium",
        "-crf",
        "20",
        "-r",
        fps.as_str(),
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-movflags",
        "+faststart",
        "-shortest",
    ];
    for value in tail {
        args.push(value.to_string());
    }
    args.push(spec.output.display().to_string());

    Ok(BuiltCommand {
        program: DEFAULT_FFMPEG_BINARY.to_string(),
        args,
        stdin: None,
    })
}

/// Build the `-filter_complex` graph (single line, `;`-separated steps).
fn build_filtergraph(spec: &AssemblySpec) -> String {
    let mut steps: Vec<String> = Vec::with_capacity(spec.segments.len() * 2 + 3);

    for index in 0..spec.segments.len() {
        steps.push(format!("[{}:v]{}[v{index}]", index * 2, video_filter(spec)));
        steps.push(format!(
            "[{}:a]aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a{index}]",
            index * 2 + 1
        ));
    }

    let concat_inputs: String = (0..spec.segments.len())
        .map(|index| format!("[v{index}][a{index}]"))
        .collect();
    steps.push(format!(
        "{concat_inputs}concat=n={}:v=1:a=1[vcat][acat]",
        spec.segments.len()
    ));

    steps.push(match &spec.subtitles {
        Some(file) => format!(
            "[vcat]subtitles={}:force_style='{SUBTITLE_STYLE}'[vout]",
            escape_filter_path(file)
        ),
        None => "[vcat]null[vout]".to_string(),
    });
    steps.push("[acat]anull[aout]".to_string());

    steps.join(";")
}

/// The per-scene video filter (scale/pad, or Ken Burns).
fn video_filter(spec: &AssemblySpec) -> String {
    let (width, height, fps) = (spec.width, spec.height, spec.fps);
    match spec.motion {
        Motion::Static => format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease,\
             pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}"
        ),
        Motion::KenBurns => {
            let zoom_width = width.saturating_mul(2);
            let zoom_height = height.saturating_mul(2);
            format!(
                "scale={zoom_width}:{zoom_height}:force_original_aspect_ratio=increase,\
                 crop={zoom_width}:{zoom_height},\
                 zoompan=z='min(max(zoom,pzoom)+0.0015,1.5)':\
                 x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':\
                 d=1:s={width}x{height}:fps={fps},setsar=1"
            )
        }
    }
}

/// Escape a path for use as an unquoted ffmpeg filter option value.
fn escape_filter_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(ch, '\\' | ':' | '\'' | ',' | '[' | ']' | ';' | '=') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Reject specs that ffmpeg cannot assemble.
fn validate(spec: &AssemblySpec) -> Result<()> {
    if spec.segments.is_empty() {
        bail!("the assembly needs at least one segment");
    }
    if spec.width == 0 || spec.height == 0 {
        bail!(
            "the output dimensions must be positive (got {}x{})",
            spec.width,
            spec.height
        );
    }
    if !spec.width.is_multiple_of(2) || !spec.height.is_multiple_of(2) {
        bail!(
            "H.264 requires even dimensions (got {}x{})",
            spec.width,
            spec.height
        );
    }
    if spec.fps == 0 {
        bail!("the frame rate must be positive");
    }
    if spec.output.as_os_str().is_empty() {
        bail!("the assembly has no output path");
    }
    for (index, segment) in spec.segments.iter().enumerate() {
        if segment.image.as_os_str().is_empty() {
            bail!("segment {index} has no image path");
        }
        if segment.audio.as_os_str().is_empty() {
            bail!("segment {index} has no audio path");
        }
        if !segment.duration_secs.is_finite() || segment.duration_secs <= 0.0 {
            bail!(
                "segment {index} has an invalid duration ({}s)",
                segment.duration_secs
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(index: usize, duration_secs: f64) -> AssemblySegment {
        AssemblySegment {
            image: PathBuf::from(format!("/work/scene-{index}.png")),
            audio: PathBuf::from(format!("/work/beat-{index}.wav")),
            duration_secs,
        }
    }

    fn spec() -> AssemblySpec {
        AssemblySpec::vertical(
            vec![segment(0, 3.2), segment(1, 4.1)],
            PathBuf::from("/work/short.mp4"),
        )
    }

    #[test]
    fn builds_inputs_filter_and_output() {
        let command = build_assembly_command(&spec()).expect("command builds");
        assert_eq!(command.program, "ffmpeg");
        assert_eq!(command.stdin, None);

        let filter = command
            .args
            .iter()
            .position(|arg| arg == "-filter_complex")
            .and_then(|index| command.args.get(index + 1))
            .expect("filtergraph present");

        assert!(filter.contains("[0:v]scale=1080:1920"));
        assert!(filter.contains("[2:v]scale=1080:1920"));
        assert!(filter.contains("concat=n=2:v=1:a=1"));
        assert!(filter.contains("[1:a]aformat="));
        assert!(filter.contains("[3:a]aformat="));
        assert!(filter.contains("[vcat]null[vout]"));
        assert!(filter.contains("[acat]anull[aout]"));

        assert!(command.args.contains(&"-loop".to_string()));
        assert!(command.args.contains(&"-framerate".to_string()));
        assert!(command.args.contains(&"3.200".to_string()));
        assert_eq!(
            command.args.last().map(String::as_str),
            Some("/work/short.mp4")
        );
        assert!(command.args.contains(&"libx264".to_string()));
        assert!(command.args.contains(&"yuv420p".to_string()));
    }

    #[test]
    fn burns_subtitles_when_present() {
        let mut spec = spec();
        spec.subtitles = Some(PathBuf::from("/work/subtitles.srt"));
        let command = build_assembly_command(&spec).expect("command builds");
        let filter = command
            .args
            .iter()
            .position(|arg| arg == "-filter_complex")
            .and_then(|index| command.args.get(index + 1))
            .expect("filtergraph present");
        assert!(filter.contains("subtitles=/work/subtitles.srt:force_style="));
    }

    #[test]
    fn ken_burns_uses_zoompan() {
        let mut spec = spec();
        spec.motion = Motion::KenBurns;
        let command = build_assembly_command(&spec).expect("command builds");
        let filter = command
            .args
            .iter()
            .position(|arg| arg == "-filter_complex")
            .and_then(|index| command.args.get(index + 1))
            .expect("filtergraph present");
        assert!(filter.contains("zoompan=z='min(max(zoom,pzoom)"));
        assert!(filter.contains("s=1080x1920"));
    }

    #[test]
    fn rejects_an_empty_segment_list() {
        let spec = AssemblySpec::vertical(Vec::new(), PathBuf::from("out.mp4"));
        assert!(build_assembly_command(&spec).is_err());
    }

    #[test]
    fn rejects_odd_dimensions() {
        let mut spec = spec();
        spec.height = 1921;
        assert!(build_assembly_command(&spec).is_err());
    }

    #[test]
    fn rejects_a_missing_audio() {
        let mut spec = spec();
        spec.segments[1].audio = PathBuf::new();
        assert!(build_assembly_command(&spec).is_err());
    }

    #[test]
    fn rejects_a_non_positive_duration() {
        let mut spec = spec();
        spec.segments[0].duration_secs = 0.0;
        assert!(build_assembly_command(&spec).is_err());
    }

    #[test]
    fn motion_parses_its_labels() {
        assert_eq!("static".parse::<Motion>().expect("static"), Motion::Static);
        assert_eq!(
            "kenburns".parse::<Motion>().expect("kenburns"),
            Motion::KenBurns
        );
        assert!("nope".parse::<Motion>().is_err());
    }
}
