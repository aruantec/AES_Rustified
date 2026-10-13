//! Persistent sparse cache for remote streams — downloaded ranges land
//! in a per-item data file (`Cache/Streams/{hash}.bin` + `{hash}.json`
//! meta) so seeks over fetched bytes are free, the waveform worker can
//! consume the same bytes as playback (no second connection — the stall
//! source), and a replay resumes exactly where the last session stopped
//! until the whole file is cached.
//!
//! Readers lock `inner()` and either take bytes via `covered_read` or
//! wait on `ready` while a live downloader feeds ahead of them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<StreamCache>>>> = OnceLock::new();
static LIVE_SEQ: AtomicU64 = AtomicU64::new(1);

const META_VERSION: u32 = 1;
const SAVE_INTERVAL_MS: u128 = 2_000;

/// Monotonic id for the currently live downloader of a cache item —
/// stale feeds (replaced on seek) must not touch the live record.
pub fn next_live_id() -> u64 {
    LIVE_SEQ.fetch_add(1, Ordering::Relaxed)
}

#[derive(Serialize, Deserialize)]
struct Meta {
    v: u32,
    len: Option<u64>,
    /// Resolved-format signature (googlevideo `itag`/`clen`) — a
    /// different format on the same item invalidates cached bytes.
    sig: String,
    ranges: Vec<(u64, u64)>,
    /// Loudness measured by the on-the-fly ReplayGain pass (mean
    /// level dBFS) — survives restarts like the byte ranges do, so a
    /// replay applies normalization instantly instead of waiting for
    /// enough audio to decode again.
    #[serde(default)]
    rg_mean_db: Option<f64>,
}

/// Live downloader record inside `Inner`: consumers wait while it
/// feeds at-or-ahead of their position.
pub struct Live {
    id: u64,
    /// Offset the feed started at — consumers below `base` are in a
    /// gap the live feed will never fill.
    base: u64,
    workers: usize,
    done: bool,
    err: bool,
}

pub struct Inner {
    file: std::fs::File,
    /// Sorted, non-overlapping `[start, end)` byte ranges on disk.
    ranges: Vec<(u64, u64)>,
    len: Option<u64>,
    sig: String,
    live: Option<Live>,
    /// See `Meta::rg_mean_db`.
    rg_mean_db: Option<f64>,
    last_save: Instant,
}

impl Inner {
    /// Byte range containing `pos` as a slice end, or None.
    pub fn covered_end(&self, pos: u64) -> Option<u64> {
        self.ranges
            .iter()
            .find(|(s, e)| *s <= pos && pos < *e)
            .map(|(_, e)| *e)
    }

    /// Serve from the data file when `pos` is inside a fetched range.
    pub fn covered_read(&mut self, pos: u64, buf: &mut [u8]) -> Option<usize> {
        let end = self.covered_end(pos)?;
        let n = ((end - pos) as usize).min(buf.len());
        self.file.seek(SeekFrom::Start(pos)).ok()?;
        let n = self.file.read(&mut buf[..n]).ok()?;
        if n == 0 { None } else { Some(n) }
    }

    /// Total stream length once the feeder's headers landed.
    pub fn total_len(&self) -> Option<u64> {
        self.len
    }

    /// Sum of cached bytes across all ranges — a cheap progress
    /// signal for consumers waiting on a live feed.
    pub fn ranges_total(&self) -> u64 {
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }

    /// `(base, done, err)` of the live downloader.
    pub fn live(&self) -> Option<(u64, bool, bool)> {
        self.live.as_ref().map(|l| (l.base, l.done, l.err))
    }

    fn add_range(&mut self, mut s: u64, mut e: u64) {
        // Merge into neighbours / overlapping ranges — the set stays
        // small so a linear merge is fine.
        let mut i = 0;
        while i < self.ranges.len() {
            let (rs, re) = self.ranges[i];
            if e < rs || s > re {
                i += 1;
                continue;
            }
            s = s.min(rs);
            e = e.max(re);
            self.ranges.remove(i);
        }
        let at = self.ranges.partition_point(|(rs, _)| *rs <= s);
        self.ranges.insert(at, (s, e));
    }

    fn save_meta(&self, meta_path: &PathBuf) {
        let meta = Meta {
            v: META_VERSION,
            len: self.len,
            sig: self.sig.clone(),
            ranges: self.ranges.clone(),
            rg_mean_db: self.rg_mean_db,
        };
        if let Ok(json) = serde_json::to_string(&meta) {
            let tmp = meta_path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, meta_path);
            }
        }
    }
}

pub struct StreamCache {
    inner: Mutex<Inner>,
    /// Woken on every append and every live-state change.
    ready: Condvar,
    meta_path: PathBuf,
}

impl StreamCache {
    pub fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn ready(&self) -> &Condvar {
        &self.ready
    }

    pub fn len(&self) -> Option<u64> {
        self.inner.lock().unwrap().len
    }

    /// The ReplayGain loudness measured on an earlier playthrough, if
    /// one completed far enough to be representative.
    pub fn rg_mean_db(&self) -> Option<f64> {
        self.inner().rg_mean_db
    }

    /// Persist the measured loudness once analysis finished — written
    /// immediately rather than on the save-interval cadence since a
    /// track change can end this session at any moment.
    pub fn set_rg_mean_db(&self, db: f64) {
        let mut inner = self.inner();
        inner.rg_mean_db = Some(db);
        inner.save_meta(&self.meta_path);
    }

    /// Called by a feeder once the resolved stream's headers land —
    /// a changed total length or format signature wipes stale bytes.
    pub fn ensure_format(&self, len: Option<u64>, sig: &str) {
        let mut inner = self.inner();
        let mismatch = inner.sig != sig
            || matches!((inner.len, len), (Some(a), Some(b)) if a != b);
        if mismatch {
            inner.ranges.clear();
            let _ = inner.file.set_len(0);
            let _ = inner.file.seek(SeekFrom::Start(0));
        }
        inner.len = len;
        inner.sig = sig.to_string();
    }

    /// A downloader begins at `base` with `workers` lanes.
    pub fn live_begin(&self, id: u64, base: u64, workers: usize) {
        self.inner().live = Some(Live {
            id,
            base,
            workers,
            done: false,
            err: false,
        });
        self.ready.notify_all();
    }

    /// A downloader lane exited; the feed is `done` when the last one
    /// leaves. Stale ids (pre-seek feeds) are ignored.
    pub fn live_worker_done(&self, id: u64, err: bool) {
        let mut inner = self.inner();
        if let Some(l) = inner.live.as_mut() {
            if l.id == id {
                l.err |= err;
                l.workers = l.workers.saturating_sub(1);
                if l.workers == 0 {
                    l.done = true;
                }
            }
        }
        drop(inner);
        self.ready.notify_all();
    }

    /// The feeder's source went away (track change / stop) — consumers
    /// waiting on it must wake rather than hang.
    pub fn clear_live(&self, id: u64) {
        let mut inner = self.inner();
        if matches!(inner.live, Some(ref l) if l.id == id) {
            inner.live = None;
        }
        drop(inner);
        self.ready.notify_all();
    }

    /// Append bytes at the absolute stream offset `off` (called by a
    /// downloader lane) and mark them served.
    pub fn append(&self, off: u64, bytes: &[u8]) {
        let mut inner = self.inner();
        let _ = inner.file.seek(SeekFrom::Start(off));
        if inner.file.write_all(bytes).is_err() {
            return;
        }
        inner.add_range(off, off + bytes.len() as u64);
        if inner.last_save.elapsed().as_millis() >= SAVE_INTERVAL_MS {
            inner.save_meta(&self.meta_path);
            inner.last_save = Instant::now();
        }
        drop(inner);
        self.ready.notify_all();
    }
}

/// FNV-1a-64 of the lowercase key — filename-safe id shared with the
/// metadata/thumbnail caches.
pub(crate) fn hash_key(key: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in key.to_lowercase().as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// `itag=`/`clen=` param — format signature for cache validation.
pub fn format_sig(url: &str) -> String {
    for part in url.split(&['?', '&'][..]) {
        if part.starts_with("itag=") || part.starts_with("clen=") {
            return part.to_string();
        }
    }
    String::new()
}

/// Shared cache for an item (`key` = the page/item url — stable across
/// resolves, unlike the signed media url).
pub fn cache_for(key: &str) -> Arc<StreamCache> {
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut reg = registry.lock().unwrap();
    if let Some(c) = reg.get(key) {
        return c.clone();
    }
    let c = Arc::new(StreamCache::load(key));
    reg.insert(key.to_string(), c.clone());
    c
}

/// Drop an item's cached stream data: evict the live registry entry
/// (a future `cache_for` reloads empty state, and any still-open file
/// handle detaches with the last Arc) and remove its bin/meta files.
/// Used when the item is deleted or its album cleared — the per-key
/// data is dead weight after that.
pub fn delete(key: &str) {
    if let Some(reg) = REGISTRY.get() {
        reg.lock().unwrap().remove(key);
    }
    let dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AES_Rustified")
        .join("Cache")
        .join("Streams");
    let id = format!("{:016x}", hash_key(key));
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    let _ = std::fs::remove_file(dir.join(format!("{id}.bin")));
}

impl StreamCache {
    fn load(key: &str) -> Self {
        let dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AES_Rustified")
            .join("Cache")
            .join("Streams");
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("{:016x}", hash_key(key));
        let meta_path = dir.join(format!("{id}.json"));
        let data_path = dir.join(format!("{id}.bin"));

        let meta: Option<Meta> = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Meta>(&t).ok())
            .filter(|m| m.v == META_VERSION);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&data_path)
            .expect("stream cache file");

        let (len, sig, ranges, rg_mean_db) = meta
            .map(|m| (m.len, m.sig, m.ranges, m.rg_mean_db))
            .unwrap_or((None, String::new(), Vec::new(), None));

        Self {
            inner: Mutex::new(Inner {
                file,
                ranges,
                len,
                sig,
                live: None,
                rg_mean_db,
                last_save: Instant::now(),
            }),
            ready: Condvar::new(),
            meta_path,
        }
    }
}
