//! AES Online Playlist (.apl) files — Lacrima's
//! `AplPlaylistDocument` JSON: a named list of online media URLs with
//! lightweight display metadata. Only URL items are ever persisted.
//!
//! ```json
//! { "format": "AES Online Playlist", "version": 1, "name": "...",
//!   "items": [{ "url": "...", "title": "...", "artist": "...",
//!               "album": "...", "duration": 0.0, "track": 0 }] }
//! ```

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Default)]
pub struct Document {
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub items: Vec<Item>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Item {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub track: u32,
}

fn default_format() -> String {
    "AES Online Playlist".into()
}
fn default_version() -> u32 {
    1
}

/// Parse an .apl file. Errors on malformed JSON; empty item lists are
/// the caller's concern (Lacrima warns and aborts the import).
pub fn read(path: &Path) -> std::io::Result<Document> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Write the playlist document (pretty — the files are hand-editable).
pub fn write(path: &Path, name: &str, items: Vec<Item>) -> std::io::Result<()> {
    let doc = Document {
        format: default_format(),
        version: default_version(),
        name: Some(name.to_string()),
        items,
    };
    let json = serde_json::to_string_pretty(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    std::fs::write(path, json)
}

/// Lacrima's SanitizeFileName — Windows-invalid chars become `_`,
/// edge spaces/dots trimmed.
pub fn sanitize_file_name(name: &str) -> String {
    const INVALID: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let s: String = name
        .trim()
        .chars()
        .map(|c| if INVALID.contains(&c) || c.is_control() { '_' } else { c })
        .collect();
    s.trim_matches(|c| c == ' ' || c == '.').to_string()
}
