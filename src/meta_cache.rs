//! Per-item metadata + thumbnail cache for online media — the small
//! Rust equivalent of Lacrima's `.meta` store (`BinaryMetadataHelper`
//! + `MetadataPathHelper`). Resolved labels and cover bytes persist
//! under `Cache/Metadata` + `Cache/Covers` so reopening an album never
//! re-fetches them; keys normalize through `clean_video_link` so the
//! same video under watch/youtu.be/playlist spellings shares an entry.
//! Misses are never cached — transient failures (rate limits, dead
//! links) self-heal on the next load.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct CachedMeta {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub thumb: Option<String>,
}

fn cache_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AES_Rustified")
        .join("Cache")
}

/// Filesystem key for an item/thumbnail url.
fn key_for(url: &str) -> String {
    let norm = crate::ytdlp::clean_video_link(url);
    format!(
        "{:016x}",
        crate::audio::stream_cache::hash_key(&norm)
    )
}

fn meta_path(url: &str) -> PathBuf {
    cache_dir()
        .join("Metadata")
        .join(format!("{}.json", key_for(url)))
}

fn image_path(url: &str) -> PathBuf {
    cache_dir()
        .join("Covers")
        .join(format!("{}.img", key_for(url)))
}

/// Cached label set for a stream url — `None` when never resolved.
pub fn load(url: &str) -> Option<CachedMeta> {
    let text = std::fs::read_to_string(meta_path(url)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Persist resolved labels (tmp + rename — a crashed write never
/// serves a half file).
pub fn save(url: &str, meta: &CachedMeta) {
    let path = meta_path(url);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(json) = serde_json::to_string(meta) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Cached thumbnail bytes for a thumbnail url (the fetched image —
/// bar-cropping still happens at decode time).
pub fn load_image(url: &str) -> Option<Vec<u8>> {
    std::fs::read(image_path(url)).ok()
}

pub fn save_image(url: &str, bytes: &[u8]) {
    let path = image_path(url);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Drop a cached thumbnail whose bytes no longer decode — the next
/// fetch repopulates it.
pub fn remove_image(url: &str) {
    let _ = std::fs::remove_file(image_path(url));
}

/// Evict an item's metadata entry and its recorded thumbnail (the
/// item was deleted — per-url entries are dead weight after that).
pub fn remove(url: &str) {
    if let Some(meta) = load(url) {
        if let Some(thumb) = meta.thumb {
            remove_image(&thumb);
        }
    }
    let _ = std::fs::remove_file(meta_path(url));
}
