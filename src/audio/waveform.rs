//! Waveform overview generation (Lacrima's
//! AudioPlayer.GenerateWaveformAsync): decode the track on a worker
//! thread and fold the PCM into `BUCKETS` peak amplitudes, normalized
//! by the global max with a ×1.1 gain and a 0.01 floor
//! (NormalizeWaveform). The result arrives on the receiver; `cancel`
//! aborts early when the track changes underneath us.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;
use symphonia::default::{get_codecs, get_probe};

use super::player::patch_media;
use super::stream_cache::StreamCache;

/// Peaks across the whole track (Lacrima's WaveformResolution 4000).
pub const BUCKETS: usize = 4000;
/// Peak slots per second before bucketing (20ms granularity).
const SLOTS_PER_SEC: u32 = 50;
/// Regeneration attempts when a decode stops short of the declared
/// length — a stream that's still downloading or a file mid-write
/// gets re-run until its waveform actually covers the song.
const MAX_ATTEMPTS: usize = 4;

// ---------------------------------------------------------------------------
// Disk cache — `ApplicationPaths.CacheDirectory/Waveform/{hash}_{buckets}.json`.
// Generated buckets persist between runs; a changed file (size/mtime),
// a bucket-count change, or a song whose real duration no longer
// matches the stored one invalidates the entry.
// ---------------------------------------------------------------------------

const CACHE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CacheEntry {
    version: u32,
    buckets: usize,
    size: u64,
    modified: u64,
    /// Header duration the buckets were generated against — a song
    /// re-probing to a different length drops the entry so the
    /// waveform is regenerated for the actual audio.
    #[serde(default)]
    dur_secs: f64,
    waveform: Vec<f32>,
}

/// Lacrima hashes the path with SHA1 (`BinaryMetadataHelper.GetCacheId`);
/// an FNV-1a-64 hex id is equivalent for a filename-safe unique key.
fn cache_id(path: &Path) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in path.to_string_lossy().to_lowercase().as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cache_path(path: &Path) -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("AES_Oxide")
            .join("Cache")
            .join("Waveform")
            .join(format!("{:016x}_{}.json", cache_id(path), BUCKETS)),
    )
}

fn file_stamp(path: &Path) -> (u64, u64) {
    let Ok(m) = std::fs::metadata(path) else {
        return (0, 0);
    };
    let modified = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (m.len(), modified)
}

/// Header-only probe for the declared audio duration — cheap (no
/// decode) so a cached waveform can be checked against the song's
/// actual length on every load.
fn probe_duration(media: Box<dyn MediaSource>) -> Option<f64> {
    let mss = MediaSourceStream::new(media, Default::default());
    let format = get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    format
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .is_some_and(|a| {
                    a.codec != CODEC_ID_NULL_AUDIO && a.sample_rate.is_some()
                })
        })
        .and_then(|t| t.num_frames.and_then(|nf| t.time_base.map(|tb| (nf, tb))))
        .and_then(|(nf, tb)| tb.calc_time(Timestamp::new(nf as i64)))
        .map(|t| t.as_secs_f64())
        .filter(|d| *d > 0.1)
}

fn load_cache(path: &Path) -> Option<Vec<f32>> {
    let p = cache_path(path)?;
    let text = std::fs::read_to_string(&p).ok()?;
    let entry = serde_json::from_str::<CacheEntry>(&text).ok()?;
    let (size, modified) = file_stamp(path);
    if entry.version == CACHE_VERSION
        && entry.buckets == BUCKETS
        && entry.waveform.len() == BUCKETS
        && entry.size == size
        && entry.modified == modified
    {
        // The waveform must match the song's real length — an entry
        // generated against a different duration is dropped so the
        // next play regenerates it instead of serving a stale shape.
        if entry.dur_secs > 0.0 {
            if let Ok(f) = std::fs::File::open(path) {
                if let Some(d) = probe_duration(patch_media(Box::new(f))) {
                    if (entry.dur_secs - d).abs() > (0.05 * d).max(1.0) {
                        let _ = std::fs::remove_file(&p);
                        return None;
                    }
                }
            }
        }
        return Some(entry.waveform);
    }
    // Stale for a changed file or a different bucket count — drop it
    // like Lacrima so it can't be resurrected by a future read.
    let _ = std::fs::remove_file(&p);
    None
}

fn save_cache(path: &Path, g: &Gen, waveform: &[f32]) {
    let Some(p) = cache_path(path) else { return };
    let (size, modified) = file_stamp(path);
    let entry = CacheEntry {
        version: CACHE_VERSION,
        buckets: BUCKETS,
        size,
        modified,
        dur_secs: g.dur_secs.unwrap_or(0.0),
        waveform: waveform.to_vec(),
    };
    let Ok(json) = serde_json::to_string(&entry) else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // tmp + rename: a half-written file can never be served.
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Decode `path` into peak buckets on a worker thread. The finished
/// `BUCKETS`-long vector arrives on the receiver — served straight from
/// the disk cache when available; nothing arrives if decoding fails or
/// `cancel` flips. A decode that ends short of the song's declared
/// length is retried so a file that was still being written fills in.
pub fn spawn(path: PathBuf, cancel: Arc<AtomicBool>) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-waveform".into())
        .spawn(move || {
            if let Some(w) = load_cache(&path) {
                let _ = tx.send(w);
                return;
            }
            let mut g = Gen::default();
            for attempt in 0..MAX_ATTEMPTS {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let ok = std::fs::File::open(&path)
                    .ok()
                    .map(|f| generate(patch_media(Box::new(f)), &cancel, &tx, &mut g))
                    .unwrap_or(false);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if !ok || g.complete() || attempt + 1 >= MAX_ATTEMPTS {
                    break;
                }
                // Short back-off — the file may still be filling in.
                std::thread::sleep(Duration::from_millis(400));
            }
            if let Some(w) = g.finish() {
                // Only complete coverage is persisted — a truncated
                // result must regenerate on the next play rather than
                // cache the mismatch.
                if g.complete() {
                    save_cache(&path, &g, &w);
                }
                let _ = tx.send(w);
            }
        })
        .expect("waveform thread");
    rx
}

/// Same as `spawn` but decodes a resolved stream URL as a read-only
/// consumer of the shared stream cache — no second download, and the
/// waveform fills at disk speed for already-fetched bytes, then at the
/// live download rate. `key` is the stable item url.
///
/// When a pass ends before the song's declared length — the download
/// stalled or the demuxer hit a gap — the worker waits for the shared
/// cache to gain coverage and regenerates, merging peaks by absolute
/// time so earlier passes' data is never lost (the waveform resumes
/// where it left off rather than restarting visually).
pub fn spawn_stream(
    url: String,
    key: String,
    cancel: Arc<AtomicBool>,
) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-waveform".into())
        .spawn(move || {
            let cache = super::stream_cache::cache_for(&key);
            let mut g = Gen::default();
            for attempt in 0..MAX_ATTEMPTS {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let ok = super::player::http_source(&url, &key, false)
                    .ok()
                    .map(|s| generate(s, &cancel, &tx, &mut g))
                    .unwrap_or(false);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if (ok && g.complete()) || attempt + 1 >= MAX_ATTEMPTS {
                    break;
                }
                if !wait_for_more(&cache, &cancel) {
                    break;
                }
            }
            if let Some(w) = g.finish() {
                let _ = tx.send(w);
            }
        })
        .ok();
    rx
}

/// Another generation pass only makes sense when new bytes can still
/// arrive: an active feed, or coverage that grows during the grace
/// window. Returns false when nothing more can come (dead feed and an
/// incomplete cache) so the caller finalizes with what decoded.
fn wait_for_more(cache: &Arc<StreamCache>, cancel: &AtomicBool) -> bool {
    let start = cache.inner().ranges_total();
    if matches!(cache.len(), Some(l) if start >= l) {
        // Fully cached — an immediate re-run is cheap and may pass
        // where a mid-download pass hit a gap.
        return true;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let inner = cache.inner();
        let covered = inner.ranges_total();
        if covered > start {
            return true;
        }
        match inner.live() {
            // Feed still running — data is (slowly) coming; keep
            // waiting out the stall until the deadline.
            Some((_, false, _)) => {
                drop(inner);
                std::thread::sleep(Duration::from_millis(250));
            }
            _ => {
                // `cache.len()` locks inner — this guard MUST be
                // dropped first: std::sync::Mutex is non-recursive,
                // so locking while held deadlocks this thread forever
                // AND wedges every later inner() (decode's
                // ensure_format, feed appends) on this cache.
                drop(inner);
                return matches!(cache.len(), Some(l) if covered >= l);
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

/// Accumulated peaks across generation attempts. `slots` indexes
/// absolute 20ms positions, so a restarted decode only adds the
/// positions the earlier pass never reached — re-decoded audio lands
/// on the same indices and merges by max.
struct Gen {
    /// Per-20ms mono peaks by absolute time index — kept in both
    /// mapping modes so a short result can be re-scaled over its real
    /// span at the end.
    slots: Vec<f32>,
    /// Buckets mapped by the declared duration (live-fill path).
    buckets: Vec<f32>,
    global_max: f32,
    /// Header-declared song length — the reference a generated
    /// waveform is checked against.
    dur_secs: Option<f64>,
    /// Longest decoded span across attempts, in seconds.
    decoded_secs: f64,
}

impl Default for Gen {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            buckets: vec![0.0; BUCKETS],
            global_max: 0.0,
            dur_secs: None,
            decoded_secs: 0.0,
        }
    }
}

impl Gen {
    /// True when the decoded audio covers (essentially) the declared
    /// song length — a waveform worth showing as final or caching.
    fn complete(&self) -> bool {
        match self.dur_secs {
            Some(d) => self.decoded_secs + 1.0 >= d || self.decoded_secs >= d * 0.97,
            None => !self.slots.is_empty(),
        }
    }

    /// Fold the accumulated peaks into the final normalized buckets —
    /// `buckets` when the declared-length mapping completed, else the
    /// slots spread over their real decoded span so the waveform still
    /// covers the bar end-to-end (a lying header or a permanently
    /// short stream still gets a proper shape).
    fn finish(&self) -> Option<Vec<f32>> {
        if self.slots.is_empty() {
            return None;
        }
        let mut buckets = if self.dur_secs.is_some() && self.complete() {
            self.buckets.clone()
        } else {
            let n = self.slots.len();
            let mut b = vec![0f32; BUCKETS];
            for (i, &v) in self.slots.iter().enumerate() {
                let k = (i * BUCKETS / n).min(BUCKETS - 1);
                if v > b[k] {
                    b[k] = v;
                }
            }
            b
        };
        let g = if self.global_max > 0.0 {
            self.global_max
        } else {
            1.0
        };
        for v in &mut buckets {
            *v = ((*v / g) * 1.1).clamp(0.01, 1.0);
        }
        Some(buckets)
    }
}

/// One decode pass over `media`, folding peaks into `g`. Returns false
/// when probing/setup fails (a retry won't fix it); a pass that ran
/// but ended short of the declared length leaves `g.complete()` false.
fn generate(
    media: Box<dyn MediaSource>,
    cancel: &AtomicBool,
    progress: &mpsc::Sender<Vec<f32>>,
    g: &mut Gen,
) -> bool {
    let mss = MediaSourceStream::new(media, Default::default());
    let Ok(mut format) = get_probe().probe(
        &Hint::new(),
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) else {
        return false;
    };
    let Some(track) = format
        .tracks()
        .iter()
        .find(|t| {
            t.codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .is_some_and(|a| {
                    a.codec != CODEC_ID_NULL_AUDIO && a.sample_rate.is_some()
                })
        })
    else {
        return false;
    };
    let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) else {
        return false;
    };
    let track_id = track.id;
    let Some(src_rate) = params.sample_rate else {
        return false;
    };
    // Total duration from the header maps decode time -> bucket 1:1,
    // so live snapshots fill left-to-right at their final positions
    // (Lacrima streams buckets the same way while ffmpeg decodes).
    let dur_secs = track
        .num_frames
        .and_then(|nf| track.time_base.map(|tb| (nf, tb)))
        .and_then(|(nf, tb)| tb.calc_time(Timestamp::new(nf as i64)))
        .map(|t| t.as_secs_f64())
        .filter(|d| *d > 0.1);
    // A re-probed duration that moved means bucket positions computed
    // against the old length are wrong — start the accumulation over.
    if let (Some(old), Some(new)) = (g.dur_secs, dur_secs) {
        if (old - new).abs() > 0.05 {
            *g = Gen::default();
        }
    }
    g.dur_secs = dur_secs;
    let Ok(mut decoder) =
        get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default())
    else {
        return false;
    };

    // Peak |mono| per fixed time slot; slots index absolute 20ms
    // positions so re-runs merge rather than duplicate.
    let slot_frames = (src_rate / SLOTS_PER_SEC).max(1) as usize;
    let mut slot_fill = 0usize;
    let mut slot_peak = 0f32;
    let mut slot_idx = 0usize;
    let mut last_send = Instant::now();

    let mut close_slot = |g: &mut Gen, slot_peak: f32, slot_idx: usize| {
        g.global_max = g.global_max.max(slot_peak);
        if g.slots.len() <= slot_idx {
            g.slots.resize(slot_idx + 1, 0.0);
        }
        if slot_peak > g.slots[slot_idx] {
            g.slots[slot_idx] = slot_peak;
        }
        if let Some(dur) = g.dur_secs {
            // Streaming mode: the bucket's position is final,
            // so push throttled snapshots as peaks land.
            let t = slot_idx as f64 / SLOTS_PER_SEC as f64;
            let b = ((t / dur * BUCKETS as f64) as usize).min(BUCKETS - 1);
            if slot_peak > g.buckets[b] {
                g.buckets[b] = slot_peak;
            }
            if last_send.elapsed().as_millis() > 120 && g.global_max > 0.0 {
                let snap: Vec<f32> = g
                    .buckets
                    .iter()
                    .map(|&v| ((v / g.global_max) * 1.1).clamp(0.0, 1.0))
                    .collect();
                let _ = progress.send(snap);
                last_send = Instant::now();
            }
        }
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Ok(None) | Err(_) => break,
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded: GenericAudioBufferRef = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = decoded.spec().clone();
        if decoded.frames() == 0 {
            continue;
        }
        let nch = spec.channels().count().max(1);
        let mut samples = Vec::with_capacity(decoded.frames() * nch);
        decoded.copy_to_vec_interleaved(&mut samples);
        for frame in samples.chunks(nch) {
            // Lacrima mixes down to mono (-ac 1) before peaking.
            let mono: f32 = frame.iter().sum::<f32>() / frame.len() as f32;
            slot_peak = slot_peak.max(mono.abs());
            slot_fill += 1;
            if slot_fill >= slot_frames {
                close_slot(g, slot_peak, slot_idx);
                slot_peak = 0.0;
                slot_fill = 0;
                slot_idx += 1;
            }
        }
    }
    if slot_fill > 0 {
        close_slot(g, slot_peak, slot_idx);
        slot_idx += 1;
    }
    g.decoded_secs = g
        .decoded_secs
        .max(slot_idx as f64 / SLOTS_PER_SEC as f64);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `wait_for_more`'s no-live arm used to call
    /// `cache.len()` while still holding `cache.inner()` — locking a
    /// non-recursive std Mutex twice on one thread deadlocked the
    /// waveform worker while holding the cache guard, which wedged
    /// every later `inner()` (decode's `ensure_format`, lane appends)
    /// — the "stream plays nothing after a bad cache" freeze. A
    /// completed/dead live feed lands exactly in that arm.
    #[test]
    fn wait_for_more_does_not_self_lock() {
        let key = format!("wfm-selflock-{}", std::process::id());
        let cache = super::super::stream_cache::cache_for(&key);
        // A finished downloader leaves live()=done → the `_` arm that
        // previously called cache.len() under the held guard.
        cache.live_begin(1, 0, 1);
        cache.live_worker_done(1, true);
        let cancel = AtomicBool::new(false);
        let c2 = cache.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(wait_for_more(&c2, &cancel));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("wait_for_more wedged on a recursive inner() lock");
        super::super::stream_cache::delete(&key);
    }
}
