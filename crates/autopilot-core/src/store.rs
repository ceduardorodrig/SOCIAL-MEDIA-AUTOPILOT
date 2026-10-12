//! Helpers for the local, dependency-light stores.
//!
//! The pipeline persists local state (the quota ledger, metrics samples, memory
//! proposals) as plain files rather than a database. These helpers keep the
//! write path identical everywhere: create the parent directory, then append a
//! JSON line or replace a whole document.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

/// Append one serialized value as a line to an append-only JSONL file.
///
/// `label` only names the store in error messages (for example `"ledger"` or
/// `"metrics"`), so a failure points at the right file.
pub async fn append_line<T: Serialize>(path: &Path, value: &T, label: &str) -> Result<()> {
    ensure_parent(path, label).await?;

    let mut line =
        serde_json::to_string(value).with_context(|| format!("serializing {label} entry"))?;
    line.push('\n');

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("opening {label} {}", path.display()))?;
    file.write_all(line.as_bytes())
        .await
        .with_context(|| format!("writing {label} entry"))
}

/// Write a whole document, creating the parent directory when needed.
pub async fn write_text(path: &Path, body: &str, label: &str) -> Result<()> {
    ensure_parent(path, label).await?;
    tokio::fs::write(path, body)
        .await
        .with_context(|| format!("writing {label} {}", path.display()))
}

/// Create the parent directory of `path` when it exists and is missing.
async fn ensure_parent(path: &Path, label: &str) -> Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {label} directory {}", parent.display())),
        _ => Ok(()),
    }
}
