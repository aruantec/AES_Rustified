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

use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};

/// Peaks across the whole track (Lacrima's WaveformResolution 4000).
pub const BUCKETS: usize = 4000;
/// Peak slots per second before bucketing (20ms granularity).
const SLOTS_PER_SEC: u32 = 50;

// ---------------------------------------------------------------------------
// Disk cache — `ApplicationPaths.CacheDirectory/Waveform/{hash}_{buckets}.json`.
// Generated buckets persist between runs; a changed file (size/mtime) or a
// bucket-count change invalidates the entry.
// ---------------------------------------------------------------------------

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CacheEntry {
    version: u32,
    buckets: usize,
    size: u64,
    modified: u64,
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
            .join("AES_Rustified")
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
        return Some(entry.waveform);
    }
    // Stale for a changed file or a different bucket count — drop it
    // like Lacrima so it can't be resurrected by a future read.
    let _ = std::fs::remove_file(&p);
    None
}

fn save_cache(path: &Path, waveform: &[f32]) {
    let Some(p) = cache_path(path) else { return };
    let (size, modified) = file_stamp(path);
    let entry = CacheEntry {
        version: CACHE_VERSION,
        buckets: BUCKETS,
        size,
        modified,
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
/// `cancel` flips.
pub fn spawn(path: PathBuf, cancel: Arc<AtomicBool>) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-waveform".into())
        .spawn(move || {
            if let Some(w) = load_cache(&path) {
                let _ = tx.send(w);
                return;
            }
            let w = std::fs::File::open(&path)
                .ok()
                .and_then(|f| generate(Box::new(f), &cancel, &tx));
            if let Some(w) = w {
                save_cache(&path, &w);
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
pub fn spawn_stream(
    url: String,
    key: String,
    cancel: Arc<AtomicBool>,
) -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("aes-waveform".into())
        .spawn(move || {
            let w = super::player::http_source(&url, &key, false)
                .ok()
                .and_then(|src| generate(src, &cancel, &tx));
            if let Some(w) = w {
                let _ = tx.send(w);
            }
        })
        .ok();
    rx
}

fn generate(
    media: Box<dyn MediaSource>,
    cancel: &AtomicBool,
    progress: &mpsc::Sender<Vec<f32>>,
) -> Option<Vec<f32>> {
    let mss = MediaSourceStream::new(media, Default::default());
    let probed = get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.sample_rate.is_some())?;
    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate?;
    // Total duration from the header maps decode time -> bucket 1:1,
    // so live snapshots fill left-to-right at their final positions
    // (Lacrima streams buckets the same way while ffmpeg decodes).
    let dur_secs = track
        .codec_params
        .n_frames
        .zip(track.codec_params.time_base)
        .map(|(nf, tb)| {
            let t = tb.calc_time(nf);
            t.seconds as f64 + t.frac
        })
        .filter(|d| *d > 0.1);
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .ok()?;

    // Peak |mono| per fixed time slot, then fold into BUCKETS — slot
    // rate is constant so slot -> bucket IS time -> bucket, no need
    // to know the total frame count up front.
    let slot_frames = (src_rate / SLOTS_PER_SEC).max(1) as usize;
    let mut slots: Vec<f32> = Vec::new();
    let mut buckets = vec![0f32; BUCKETS];
    let mut slot_fill = 0usize;
    let mut slot_peak = 0f32;
    let mut slot_idx = 0usize;
    let mut global_max = 0f32;
    let mut last_send = std::time::Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded: AudioBufferRef = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = *decoded.spec();
        if decoded.frames() == 0 {
            continue;
        }
        let mut buf = SampleBuffer::<f32>::new(decoded.frames() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        let nch = spec.channels.count().max(1);
        for frame in buf.samples().chunks(nch) {
            // Lacrima mixes down to mono (-ac 1) before peaking.
            let mono: f32 = frame.iter().sum::<f32>() / frame.len() as f32;
            slot_peak = slot_peak.max(mono.abs());
            slot_fill += 1;
            if slot_fill >= slot_frames {
                global_max = global_max.max(slot_peak);
                if let Some(dur) = dur_secs {
                    // Streaming mode: the bucket's position is final,
                    // so push throttled snapshots as peaks land.
                    let t = slot_idx as f64 / SLOTS_PER_SEC as f64;
                    let b = ((t / dur * BUCKETS as f64) as usize).min(BUCKETS - 1);
                    if slot_peak > buckets[b] {
                        buckets[b] = slot_peak;
                    }
                    if last_send.elapsed().as_millis() > 120 && global_max > 0.0 {
                        let snap: Vec<f32> = buckets
                            .iter()
                            .map(|&v| ((v / global_max) * 1.1).clamp(0.0, 1.0))
                            .collect();
                        let _ = progress.send(snap);
                        last_send = std::time::Instant::now();
                    }
                } else {
                    slots.push(slot_peak);
                }
                slot_peak = 0.0;
                slot_fill = 0;
                slot_idx += 1;
            }
        }
    }
    if slot_fill > 0 {
        global_max = global_max.max(slot_peak);
        if let Some(dur) = dur_secs {
            let t = slot_idx as f64 / SLOTS_PER_SEC as f64;
            let b = ((t / dur * BUCKETS as f64) as usize).min(BUCKETS - 1);
            if slot_peak > buckets[b] {
                buckets[b] = slot_peak;
            }
        } else {
            slots.push(slot_peak);
        }
    }
    if dur_secs.is_none() {
        if slots.is_empty() {
            return None;
        }
        for (i, &v) in slots.iter().enumerate() {
            let b = (i * BUCKETS / slots.len()).min(BUCKETS - 1);
            if v > buckets[b] {
                buckets[b] = v;
            }
        }
    }
    let g = if global_max > 0.0 { global_max } else { 1.0 };
    for v in &mut buckets {
        *v = ((*v / g) * 1.1).clamp(0.01, 1.0);
    }
    Some(buckets)
}
