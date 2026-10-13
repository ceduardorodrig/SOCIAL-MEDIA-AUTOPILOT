//! Piper TTS command construction and availability probing (local, CPU-only).
//!
//! Piper is a small local neural TTS engine that runs **entirely on the CPU**,
//! so it never competes for the scarce GPU. This module only *builds* the
//! command; whether the binary is installed is answered by [`probe_piper`], and
//! nothing here requires Piper to be present.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Default Piper binary name, resolved through `PATH`.
pub const DEFAULT_PIPER_BINARY: &str = "piper";

/// How long the availability probe waits for `piper --version`.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A fully built external command (program, argv and optional stdin payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltCommand {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

impl BuiltCommand {
    /// Render the command as one copy-pasteable shell line (stdin included).
    pub fn display_line(&self) -> String {
        let mut parts = vec![shell_quote(&self.program)];
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        let line = parts.join(" ");
        match &self.stdin {
            Some(stdin) => format!("printf '%s' {} | {line}", shell_quote(stdin)),
            None => line,
        }
    }
}

/// Configuration for a Piper synthesis run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PiperConfig {
    /// Piper binary; resolved through `PATH` when it has no separator.
    #[serde(default = "default_piper_binary")]
    pub binary: String,
    /// Voice model (`.onnx`); Piper finds the matching `.json` next to it.
    #[serde(default)]
    pub model: PathBuf,
    /// Speech speed: higher is slower. `None` keeps the voice default.
    #[serde(default)]
    pub length_scale: Option<f32>,
    /// Silence inserted between sentences, in seconds.
    #[serde(default)]
    pub sentence_silence: Option<f32>,
}

fn default_piper_binary() -> String {
    DEFAULT_PIPER_BINARY.to_string()
}

impl Default for PiperConfig {
    fn default() -> Self {
        Self {
            binary: default_piper_binary(),
            model: PathBuf::new(),
            length_scale: None,
            sentence_silence: None,
        }
    }
}

impl PiperConfig {
    /// Build a config for a voice model with the default binary and tuning.
    pub fn new(model: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }
}

/// Build the Piper command that synthesises `narration` into `output`.
///
/// Piper reads the text from stdin and writes a WAV to `--output_file`.
pub fn build_tts_command(
    cfg: &PiperConfig,
    narration: &str,
    output: &Path,
) -> Result<BuiltCommand> {
    if narration.trim().is_empty() {
        bail!("cannot synthesise an empty narration");
    }
    if cfg.model.as_os_str().is_empty() {
        bail!("no Piper voice model configured");
    }
    if output.as_os_str().is_empty() {
        bail!("no output path for the synthesised WAV");
    }

    let mut args = vec![
        "--model".to_string(),
        cfg.model.display().to_string(),
        "--output_file".to_string(),
        output.display().to_string(),
    ];
    if let Some(length_scale) = cfg.length_scale {
        args.push("--length_scale".to_string());
        args.push(length_scale.to_string());
    }
    if let Some(sentence_silence) = cfg.sentence_silence {
        args.push("--sentence_silence".to_string());
        args.push(sentence_silence.to_string());
    }

    Ok(BuiltCommand {
        program: cfg.binary.clone(),
        args,
        stdin: Some(narration.trim().to_string()),
    })
}

/// Result of probing for a local Piper installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiperProbe {
    pub available: bool,
    pub path: Option<PathBuf>,
    /// Best-effort `piper --version` output; `None` when it cannot be read.
    pub version: Option<String>,
}

/// Probe for a local Piper binary and, best effort, its version.
///
/// Never fails: a missing binary is a normal, reportable state.
pub async fn probe_piper(binary: &str) -> PiperProbe {
    let Some(path) = find_executable(binary) else {
        return PiperProbe {
            available: false,
            path: None,
            version: None,
        };
    };
    PiperProbe {
        available: true,
        path: Some(path),
        version: read_version(binary).await,
    }
}

/// Locate an executable on `PATH`, or resolve a path that contains a separator.
///
/// Pure Rust: it never shells out to `which`.
pub fn find_executable(binary: &str) -> Option<PathBuf> {
    let candidate = Path::new(binary);
    if candidate.components().count() > 1 {
        return if candidate.is_file() {
            Some(candidate.to_path_buf())
        } else {
            None
        };
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

async fn read_version(binary: &str) -> Option<String> {
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new(binary).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Quote a value for a POSIX shell only when it needs quoting.
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@,+".contains(ch));
    if safe {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PiperConfig {
        PiperConfig::new("/voices/pt_BR-faber-medium.onnx")
    }

    #[test]
    fn builds_the_piper_argv_and_stdin() {
        let command = build_tts_command(&config(), "olá mundo", Path::new("/out/a.wav"))
            .expect("command builds");
        assert_eq!(command.program, "piper");
        assert_eq!(
            command.args,
            vec![
                "--model",
                "/voices/pt_BR-faber-medium.onnx",
                "--output_file",
                "/out/a.wav",
            ]
        );
        assert_eq!(command.stdin.as_deref(), Some("olá mundo"));
    }

    #[test]
    fn includes_optional_tuning_flags() {
        let mut cfg = config();
        cfg.length_scale = Some(1.2);
        cfg.sentence_silence = Some(0.1);
        let command = build_tts_command(&cfg, "texto", Path::new("a.wav")).expect("command builds");
        assert!(command.args.contains(&"--length_scale".to_string()));
        assert!(command.args.contains(&"1.2".to_string()));
        assert!(command.args.contains(&"--sentence_silence".to_string()));
        assert!(command.args.contains(&"0.1".to_string()));
    }

    #[test]
    fn rejects_empty_narration() {
        assert!(build_tts_command(&config(), "   ", Path::new("a.wav")).is_err());
    }

    #[test]
    fn rejects_a_missing_model() {
        let cfg = PiperConfig::new("");
        assert!(build_tts_command(&cfg, "texto", Path::new("a.wav")).is_err());
    }

    #[test]
    fn find_executable_reports_a_missing_binary() {
        assert!(find_executable("autopilot-definitely-not-installed-xyz").is_none());
    }

    #[test]
    fn find_executable_accepts_an_explicit_path() {
        assert!(find_executable("/bin/sh").is_some());
    }

    #[test]
    fn display_line_quotes_and_pipes_stdin() {
        let command = build_tts_command(&config(), "olá, mundo", Path::new("/out/a.wav"))
            .expect("command builds");
        let line = command.display_line();
        assert!(line.starts_with("printf '%s' 'olá, mundo' | piper "));
        assert!(line.contains("--output_file /out/a.wav"));
    }
}
