//! Provider registry loaded from YAML.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

fn default_priority() -> u32 {
    100
}

fn default_timeout_ms() -> u64 {
    15_000
}

/// Whole provider registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersConfig {
    pub providers: Vec<ProviderConfig>,
}

/// One LLM provider entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Stable identifier (e.g. `opencode`, `gemini`, `ollama`).
    pub id: String,
    pub kind: ProviderKind,
    /// Model id. For OpenCode this follows `provider/model` (e.g. `opencode/mimo-v2.6-flash-free`).
    pub model: String,
    /// Lower value = earlier in the fallback chain.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Base URL for HTTP providers (Gemini, Ollama).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Name of the environment variable holding the API key (never the key itself).
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Human-readable note about the provider role.
    #[serde(default)]
    pub role: String,
}

/// Kind of backend probed for availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Opencode,
    Gemini,
    Ollama,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Opencode => "opencode",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Ollama => "ollama",
        }
    }
}

impl ProviderConfig {
    pub fn endpoint_or_default(&self, fallback: &str) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// Load the provider registry from a YAML file.
pub async fn load_config(path: &Path) -> Result<ProvidersConfig> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading providers config {}", path.display()))?;
    let config: ProvidersConfig = serde_yaml_ng::from_str(&raw)
        .with_context(|| format!("parsing providers config {}", path.display()))?;
    Ok(config)
}
