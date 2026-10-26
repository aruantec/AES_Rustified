//! The player: owns the cpal output stream (built once) and a decode
//! thread per track. Decode -> stereo interleaved f32 -> rubato resample
//! -> SPSC ring -> output callback -> EQ -> device + spectrum tap.
//!
//! Control is all atomics/mpsc so the UI thread never blocks the audio
//! thread: pause just stops consuming (the ring back-pressures the
//! decoder), seek is a request the decode thread picks up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU32, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use super::stream_cache;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use lofty::file::TaggedFileExt;
use lofty::tag::ItemKey;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::Resampler as _;
use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;
use symphonia::default::{get_codecs, get_probe};

use super::dsp::{Denoiser, Equalizer, EQ_BANDS};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PlayerState {
    Stopped = 0,
    Playing = 1,
    Paused = 2,
    /// Reached end of stream; position stays at duration.
    Finished = 3,
}

impl PlayerState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Playing,
            2 => Self::Paused,
            3 => Self::Finished,
            _ => Self::Stopped,
        }
    }
}

enum Cmd {
    Load(Source),
    /// Wake a decoder parked at track end — the position itself is in
    /// `Shared::seek`; the command just restarts the last source so
    /// it gets applied (Repeat One / seek-after-finish).
    Seek,
    Stop,
    Quit,
}

/// What the decoder reads — a local file or a remote stream URL
/// (yt-dlp-resolved googlevideo links, which honor Range requests).
#[derive(Clone)]
enum Source {
    File(PathBuf),
    /// Resolved media url + the stable item key (page url) that
    /// identifies its persistent stream cache.
    Stream { url: String, key: String },
}

/// Shared control surface between the UI thread (Player), the decode
/// thread, and the cpal output callback — atomics only.
struct Shared {
    state: AtomicU8,
    /// User-requested volume 0..1 (UI readback; the output callback
    /// applies the configured curve + gain on top).
    volume: AtomicU32, // f32 bits
    /// Volume-curve flags: bit0 = logarithmic, bit1 = loudness
    /// compensated (Lacrima's Logarithmic/LoudnessCompensatedVolume).
    vol_curve: AtomicU8,
    /// SmoothVolumeChange — ramp effective gain instead of snapping.
    smooth_vol: AtomicBool,
    /// Active ReplayGain+preamp adjustment in dB (f32 bits); the
    /// callback folds it into the effective volume multiplicatively.
    rg_db: AtomicU32,
    /// Current ReplayGain options + a generation counter bumped on
    /// every change so the decode thread re-evaluates the current
    /// source (Lacrima's RecomputeReplayGainForCurrentAsync).
    rg_opts: Mutex<ReplayGainOptions>,
    rg_opts_seq: AtomicU64,
    /// PauseWhenVolumeIsZero / the zero-volume-induced pause flag.
    pause_on_zero: AtomicBool,
    paused_zero_vol: AtomicBool,
    /// RestartOnResumeEnabled + timeframe (f64 bits, seconds).
    restart_resume: AtomicBool,
    restart_secs: AtomicU64,
    /// Position when last manually paused + whether the current pause
    /// came from `pause()` (zero-volume pauses don't set it — Lacrima's
    /// PauseForZeroVolume skips the bookkeeping).
    pos_when_paused: AtomicU64,
    was_paused: AtomicBool,
    /// AiNoiseReduction — the decode thread runs its spectral-
    /// subtraction denoiser on the resampled stream when set.
    denoise: AtomicBool,
    band_db: [AtomicU32; 10], // f32 bits
    /// Seek request picked up by the decode thread (seconds).
    seek: Mutex<Option<f64>>,
    /// Remote-stream preroll: the output callback stays silent until
    /// the decode thread has buffered enough PCM (googlevideo refills
    /// at ~realtime — starting playback on an empty ring means it can
    /// never get ahead and crackles on every jitter).
    buffering: AtomicBool,
    /// Bumped by the decoder on track change/seek; the output callback
    /// clears the PCM ring when it sees a new value.
    flush_seq: AtomicU64,
    /// Bumped once a new source has probed in and entered the decode
    /// loop — unlike `flush_seq` (which also bumps on seeks) this
    /// means "the decoder owns a different source than at open()".
    /// The UI snapshots it at open() and a change tells it the load
    /// actually went through.
    load_gen: AtomicU64,
    /// Position bookkeeping (output-rate frames).
    pos_base: AtomicU64,
    pos_consumed: AtomicU64,
    duration: AtomicU64, // f64 bits, seconds
    /// The decoder reached the source's end — the ring may still hold
    /// seconds of undrained PCM. The output callback flips Finished
    /// only once that tail has actually played out.
    eof: AtomicBool,
}

/// Lacrima's `ReplayGainOptions` — a snapshot of the Audio-tab
/// ReplayGain settings the decode thread re-reads when bumped.
#[derive(Clone, Copy, Default)]
pub struct ReplayGainOptions {
    pub enabled: bool,
    pub use_tags: bool,
    pub analyze: bool,
    /// dB added to on-the-fly analyzed gains (ReplayGainPreampDb).
    pub preamp_analyze_db: f32,
    /// dB added to tag gains (ReplayGainTagsPreampDb).
    pub preamp_tags_db: f32,
    /// 0 = track gain first, 1 = album gain first (other is fallback).
    pub tag_source: u8,
}

enum DecodeEnd {
    Eof,
    Stopped,
    Reload(Source),
    Quit,
}

/// `AudioPlayer` is the UI-facing handle. Not `Send` (cpal stream isn't),
/// so keep it on the UI side.
pub struct AudioPlayer {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
    _stream: cpal::Stream,
    _decode: JoinHandle<()>,
    /// Post-EQ samples for the spectrum analyzer, drained by the UI.
    spectrum_ring: HeapCons<f32>,
    out_rate: u32,
}

const STATE_STOP: u8 = PlayerState::Stopped as u8;
const STATE_PLAY: u8 = PlayerState::Playing as u8;
const STATE_PAUSE: u8 = PlayerState::Paused as u8;
const STATE_FIN: u8 = PlayerState::Finished as u8;

impl AudioPlayer {
    /// Builds the output stream on the default device (f32, any channel
    /// count — we write stereo to channels 0/1) and starts the decode
    /// thread. Errors if there is no usable output device.
    pub fn new() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default audio output device")?;
        let supported = device.default_output_config()?;
        if supported.sample_format() != cpal::SampleFormat::F32 {
            return Err("audio device does not support f32 output".into());
        }
        let config = supported.config();
        let out_rate = config.sample_rate;
        let channels = config.channels as usize;
        if channels == 0 {
            return Err("audio device reports zero channels".into());
        }

        let shared = Arc::new(Shared {
            state: AtomicU8::new(STATE_STOP),
            volume: AtomicU32::new(0.8f32.to_bits()),
            vol_curve: AtomicU8::new(0),
            smooth_vol: AtomicBool::new(true),
            rg_db: AtomicU32::new(0f32.to_bits()),
            rg_opts: Mutex::new(ReplayGainOptions::default()),
            rg_opts_seq: AtomicU64::new(1),
            pause_on_zero: AtomicBool::new(false),
            paused_zero_vol: AtomicBool::new(false),
            restart_resume: AtomicBool::new(false),
            restart_secs: AtomicU64::new(10f64.to_bits()),
            pos_when_paused: AtomicU64::new(0),
            was_paused: AtomicBool::new(false),
            denoise: AtomicBool::new(false),
            band_db: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())),
            seek: Mutex::new(None),
            buffering: AtomicBool::new(false),
            flush_seq: AtomicU64::new(0),
            load_gen: AtomicU64::new(0),
            pos_base: AtomicU64::new(0),
            pos_consumed: AtomicU64::new(0),
            duration: AtomicU64::new(0),
            eof: AtomicBool::new(false),
        });

        // ~4s of stereo PCM — network streams (HttpSource reads) stall
        // in bursts, so the ring needs real headroom over the local-
        // file case to keep playback smooth.
        let ring = HeapRb::<f32>::new(out_rate as usize * 8);
        let (prod, mut cons) = ring.split();
        // Spectrum tap ring: small, drop-oldest is fine.
        let spec_ring = HeapRb::<f32>::new(out_rate as usize / 2);
        let (mut spec_prod, spec_cons) = spec_ring.split();

        // Decode thread waits for Load commands.
        let (tx, rx) = mpsc::channel::<Cmd>();
        let decode_shared = shared.clone();
        let decode = std::thread::Builder::new()
            .name("aes-audio-decode".into())
            .spawn(move || decode_thread(rx, decode_shared, prod, out_rate))?;

        let cb_shared = shared.clone();
        let mut eq = Equalizer::new(out_rate as f32);
        let mut last_flush = 0u64;
        // Effective-gain smoothing state (Lacrima's SmoothVolumeChange
        // — actually implemented here as a ~50ms exponential ramp, so
        // volume/gain changes glide instead of clicking).
        let mut smoothed = 0.0f32;
        // Per-sample ramp coefficient for a ~50ms time constant.
        let ramp = 1.0 - (-1.0f32 / (out_rate as f32 * 0.05)).exp();
        // Track-transition fades: ~500ms in after every flush (track
        // open/seek/repeat-rewind) and ~500ms out while the decoder-
        // flagged EOF tail drains, so song changes ease in and out of
        // silence instead of slamming.
        let fade_len = out_rate as usize / 2;
        let mut fade_pos = 0usize;
        let data_cb = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // Decoder bumped the flush counter (new track/seek): drop
            // any stale PCM still in the ring.
            let fseq = cb_shared.flush_seq.load(Ordering::Relaxed);
            if fseq != last_flush {
                last_flush = fseq;
                cons.clear();
                fade_pos = 0;
            }
            eq.sync_gains(&cb_shared.band_db);
            let paused = cb_shared.state.load(Ordering::Relaxed) == STATE_PAUSE;
            // Preroll gate for streams: silence without draining the
            // ring while the decoder builds its buffer.
            let muted = paused || cb_shared.buffering.load(Ordering::Relaxed);
            // Effective volume (Lacrima's UpdateAf volume chain):
            // user level -> optional curve transforms -> ReplayGain.
            let mut vol = f32::from_bits(cb_shared.volume.load(Ordering::Relaxed));
            let curve = cb_shared.vol_curve.load(Ordering::Relaxed);
            if curve & 1 != 0 {
                // Logarithmic: quadratic curve, more travel at the top.
                vol *= vol;
            }
            if curve & 2 != 0 {
                // Loudness compensation: ~2/3-power lift at low levels.
                vol = vol.powf(1.0 / 1.5);
            }
            let rg_lin =
                10f32.powf(f32::from_bits(cb_shared.rg_db.load(Ordering::Relaxed)) / 20.0);
            // Lacrima caps the combined level at mpv's volume-max 200%.
            let target = (vol * rg_lin).clamp(0.0, 2.0);
            let alpha = if cb_shared.smooth_vol.load(Ordering::Relaxed) {
                ramp
            } else {
                1.0
            };
            for frame in data.chunks_mut(channels) {
                let (mut l, mut r) = if muted {
                    (0.0, 0.0)
                } else {
                    match (cons.try_pop(), cons.try_pop()) {
                        (Some(l), Some(r)) => {
                            cb_shared.pos_consumed.fetch_add(1, Ordering::Relaxed);
                            (l, r)
                        }
                        _ => {
                            let st = cb_shared.state.load(Ordering::Relaxed);
                            if cb_shared.eof.load(Ordering::Relaxed) {
                                // The decoder ended and the ring just
                                // played its last sample — this is the
                                // audible track end (MediaEnded), not
                                // an underrun.
                                if st == STATE_PLAY {
                                    cb_shared.state.store(STATE_FIN, Ordering::Relaxed);
                                }
                            } else if st == STATE_PLAY {
                                // Underrun mid-playback — re-arm the
                                // preroll gate so the stream re-buffers
                                // like a video player instead of
                                // stuttering in and out on every hiccup.
                                cb_shared.buffering.store(true, Ordering::Relaxed);
                            }
                            (0.0, 0.0)
                        }
                    }
                };
                // Transition fades: the fade-in ramp counts frames
                // since the last flush; near EOF the remaining ring
                // frames fade the tail out. Pauses and stream preroll
                // freeze both so the ramp runs on audible audio, not
                // dead time.
                if !muted {
                    if fade_pos < fade_len {
                        fade_pos += 1;
                        // Quadratic in: perceived loudness is
                        // logarithmic, so a linear ramp gets loud
                        // early — t² holds the gain down through the
                        // first half and blooms at the end, which is
                        // what keeps loud intros from slamming.
                        let t = fade_pos as f32 / fade_len as f32;
                        let g = t * t;
                        l *= g;
                        r *= g;
                    }
                    if cb_shared.eof.load(Ordering::Relaxed) {
                        let rem = cons.occupied_len() / 2;
                        if rem < fade_len {
                            let g = rem as f32 / fade_len as f32;
                            l *= g;
                            r *= g;
                        }
                    }
                }
                eq.process(&mut l, &mut r);
                let _ = spec_prod.try_push(l);
                let _ = spec_prod.try_push(r);
                smoothed += (target - smoothed) * alpha;
                frame[0] = l * smoothed;
                if channels > 1 {
                    frame[1] = r * smoothed;
                    for s in frame.iter_mut().skip(2) {
                        *s = 0.0;
                    }
                }
            }
        };
        let stream = device.build_output_stream(
            config,
            data_cb,
            |err| eprintln!("audio output error: {err}"),
            None,
        )?;
        stream.play()?;

        Ok(Self {
            shared,
            tx,
            _stream: stream,
            _decode: decode,
            spectrum_ring: spec_cons,
            out_rate,
        })
    }

    /// Open a file and start playing it.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<()> {
        self.shared.eof.store(false, Ordering::Relaxed);
        *self.shared.seek.lock().unwrap() = None;
        self.shared.buffering.store(false, Ordering::Relaxed);
        // The new source's ReplayGain is computed by the decode thread;
        // clear the previous track's adjustment so it never leaks in.
        self.shared.rg_db.store(0f32.to_bits(), Ordering::Relaxed);
        self.shared.state.store(STATE_PLAY, Ordering::Relaxed);
        self.tx.send(Cmd::Load(Source::File(path.as_ref().to_path_buf())))?;
        Ok(())
    }

    /// Open a remote stream URL and start playing it (resolved by
    /// yt-dlp — googlevideo links support HTTP Range seeks). `key` is
    /// the stable item url identifying its persistent stream cache.
    /// The output stays muted until the decode thread fills the preroll
    /// window.
    pub fn open_stream(&self, url: &str, key: &str) -> Result<()> {
        self.shared.eof.store(false, Ordering::Relaxed);
        *self.shared.seek.lock().unwrap() = None;
        self.shared.buffering.store(true, Ordering::Relaxed);
        self.shared.rg_db.store(0f32.to_bits(), Ordering::Relaxed);
        self.shared.state.store(STATE_PLAY, Ordering::Relaxed);
        self.tx.send(Cmd::Load(Source::Stream {
            url: url.to_string(),
            key: key.to_string(),
        }))?;
        Ok(())
    }

    /// Lacrima's `Pause()`: records the position for restart-on-resume
    /// and clears the zero-volume flag (a manual pause is never the
    /// zero-volume state).
    pub fn pause(&self) {
        if self.state() == PlayerState::Playing {
            self.shared.paused_zero_vol.store(false, Ordering::Relaxed);
            self.shared
                .pos_when_paused
                .store(self.position().as_secs_f64().to_bits(), Ordering::Relaxed);
            self.shared.was_paused.store(true, Ordering::Relaxed);
            self.shared.state.store(STATE_PAUSE, Ordering::Relaxed);
        }
    }

    /// Lacrima's `Play()`: resuming a track that was paused inside the
    /// restart timeframe (0..threshold seconds in) starts it over.
    pub fn resume(&self) {
        if self.state() == PlayerState::Paused {
            if self.shared.restart_resume.load(Ordering::Relaxed)
                && self.shared.was_paused.load(Ordering::Relaxed)
                && f64::from_bits(self.shared.pos_when_paused.load(Ordering::Relaxed))
                    <= f64::from_bits(self.shared.restart_secs.load(Ordering::Relaxed))
            {
                self.seek(0.0);
            }
            self.shared.was_paused.store(false, Ordering::Relaxed);
            self.shared.state.store(STATE_PLAY, Ordering::Relaxed);
        }
    }

    pub fn toggle(&self) {
        match self.state() {
            PlayerState::Playing => self.pause(),
            PlayerState::Paused => self.resume(),
            _ => {}
        }
    }

    /// Stop playback and rewind. The decode thread idles until `open`.
    pub fn stop(&self) {
        self.shared.eof.store(false, Ordering::Relaxed);
        self.shared.state.store(STATE_STOP, Ordering::Relaxed);
        self.shared.pos_base.store(0, Ordering::Relaxed);
        self.shared.pos_consumed.store(0, Ordering::Relaxed);
        // Flush buffered PCM so audio cuts immediately rather than
        // draining the ring first.
        self.shared.flush_seq.fetch_add(1, Ordering::Relaxed);
        // Lacrima's InternalStop clears the resume/zero-volume state.
        self.shared.was_paused.store(false, Ordering::Relaxed);
        self.shared
            .pos_when_paused
            .store(0f64.to_bits(), Ordering::Relaxed);
        self.shared.paused_zero_vol.store(false, Ordering::Relaxed);
        self.shared.rg_db.store(0f32.to_bits(), Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    /// Seek to `seconds` within the current track. Also wakes a
    /// decoder parked at track end — Repeat One and seek-after-finish
    /// restart the last source and apply the position.
    pub fn seek(&self, seconds: f64) {
        *self.shared.seek.lock().unwrap() = Some(seconds.max(0.0));
        // A seek is a restart intent — the parked-decoder path clears
        // eof via the reload; clearing here keeps the output callback
        // from re-flagging Finished in the gap.
        self.shared.eof.store(false, Ordering::Relaxed);
        self.shared.state.store(STATE_PLAY, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Seek);
    }

    pub fn state(&self) -> PlayerState {
        PlayerState::from_u8(self.shared.state.load(Ordering::Relaxed))
    }

    /// Stream preroll gate — true between `open_stream` and the first
    /// ~1.5s of decoded PCM reaching the ring buffer.
    pub fn buffering(&self) -> bool {
        self.shared.buffering.load(Ordering::Relaxed)
    }

    /// Decode-loop generation: bumped once a new source probed in and
    /// started decoding. Snapshotting this at open() and comparing
    /// later tells callers the load actually applied (a failed probe
    /// never bumps it). Acquire so the position reset that precedes
    /// the bump is visible with it.
    pub fn load_gen(&self) -> u64 {
        self.shared.load_gen.load(Ordering::Acquire)
    }

    pub fn position(&self) -> Duration {
        let frames = self.shared.pos_base.load(Ordering::Relaxed)
            + self.shared.pos_consumed.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / self.out_rate as f64)
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(f64::from_bits(self.shared.duration.load(Ordering::Relaxed)))
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Relaxed))
    }

    /// 0.0..=1.0, clamped. With PauseWhenVolumeIsZero set, dragging to
    /// zero pauses and raising it again resumes — Lacrima's `Volume`
    /// setter semantics (`PauseForZeroVolume` deliberately skips the
    /// manual-pause bookkeeping).
    pub fn set_volume(&self, v: f32) {
        const EPS: f32 = 0.00001; // Lacrima's 0.001 on its 0..100 scale
        let v = v.clamp(0.0, 1.0);
        let old = self.volume();
        if (old - v).abs() < EPS {
            return;
        }
        let was_zero = old <= EPS;
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
        if !self.shared.pause_on_zero.load(Ordering::Relaxed) {
            return;
        }
        if v <= EPS && self.state() == PlayerState::Playing {
            self.shared.paused_zero_vol.store(true, Ordering::Relaxed);
            self.shared.state.store(STATE_PAUSE, Ordering::Relaxed);
        } else if was_zero
            && v > EPS
            && self.shared.paused_zero_vol.swap(false, Ordering::Relaxed)
            && self.state() == PlayerState::Paused
        {
            self.resume();
        }
    }

    /// Push the Audio settings section onto the player (Lacrima syncs
    /// the same set via property mirroring + RecomputeReplayGain).
    /// The `rg_opts_seq` bump makes the decode thread re-evaluate the
    /// current source live — tags re-read, analysis restarted.
    pub fn apply_audio(&self, a: &crate::settings::Audio) {
        let curve = (a.logarithmic_volume_control as u8)
            | ((a.loudness_compensated_volume as u8) << 1);
        self.shared.vol_curve.store(curve, Ordering::Relaxed);
        self.shared
            .smooth_vol
            .store(a.smooth_volume_change, Ordering::Relaxed);
        self.shared
            .pause_on_zero
            .store(a.pause_when_volume_is_zero, Ordering::Relaxed);
        self.shared
            .restart_resume
            .store(a.restart_on_resume_enabled, Ordering::Relaxed);
        self.set_volume(a.volume);
        self.shared.restart_secs.store(
            (a.restart_on_resume_threshold_seconds as f64).to_bits(),
            Ordering::Relaxed,
        );
        self.shared
            .denoise
            .store(a.ai_noise_reduction, Ordering::Relaxed);
        *self.shared.rg_opts.lock().unwrap() = ReplayGainOptions {
            enabled: a.replay_gain_enabled,
            use_tags: a.replay_gain_use_tags,
            analyze: a.replay_gain_analyze_on_the_fly,
            preamp_analyze_db: a.replay_gain_preamp_db,
            preamp_tags_db: a.replay_gain_tags_preamp_db,
            tag_source: a.replay_gain_tag_source,
        };
        self.shared.rg_opts_seq.fetch_add(1, Ordering::Relaxed);
    }

    /// Set one EQ band's gain in dB (clamped to ±12).
    pub fn set_band_gain(&self, band: usize, db: f32) {
        if band < EQ_BANDS.len() {
            self.shared.band_db[band]
                .store(db.clamp(-12.0, 12.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// Output device sample rate — the rate spectrum PCM arrives at.
    pub fn output_rate(&self) -> u32 {
        self.out_rate
    }

    /// Drain the post-EQ spectrum tap; feed into `Spectrum` once per frame.
    pub fn drain_spectrum(&mut self) -> Vec<f32> {
        let mut out = Vec::with_capacity(2048);
        while let Some(s) = self.spectrum_ring.try_pop() {
            out.push(s);
        }
        out
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

fn decode_thread(
    rx: mpsc::Receiver<Cmd>,
    shared: Arc<Shared>,
    mut prod: HeapProd<f32>,
    out_rate: u32,
) {
    let mut pending: Option<Source> = None;
    /// Last decoded source — a Seek while parked replays it (Repeat
    /// One, seek-after-finish) since the seek position lives in
    /// `Shared::seek` and gets applied by `run_decoder`.
    let mut last: Option<Source> = None;
    loop {
        // Idle until a track arrives.
        while pending.is_none() {
            match rx.recv() {
                Ok(Cmd::Load(s)) => pending = Some(s),
                Ok(Cmd::Seek) => pending = last.clone(),
                // Stop while parked just leaves the thread waiting —
                // exiting here would kill every later open().
                Ok(Cmd::Stop) => {}
                Ok(Cmd::Quit) | Err(_) => return,
            }
        }
        let src = pending.take().unwrap();
        last = Some(src.clone());
        match decode_source(src, &rx, &shared, &mut prod, out_rate) {
            DecodeEnd::Eof => {
                // A stream shorter than the preroll still plays what
                // arrived — ungate the output.
                shared.buffering.store(false, Ordering::Relaxed);
                // Don't flag Finished yet — the ring may still hold
                // seconds of PCM. The output callback flips it once
                // the last sample drains, so repeat/advance fires at
                // the audible end.
                shared.eof.store(true, Ordering::Relaxed);
            }
            DecodeEnd::Stopped => {
                // A failed open (unprobeable stream, dead URL) must
                // unlatch the preroll gate — open_stream sets it and
                // only decoded PCM clears it, so leaving it latched
                // mutes every subsequent source too.
                shared.buffering.store(false, Ordering::Relaxed);
            }
            DecodeEnd::Reload(s) => pending = Some(s),
            DecodeEnd::Quit => return,
        }
        if pending.is_none()
            && shared.state.load(Ordering::Relaxed) != STATE_FIN
            && !shared.eof.load(Ordering::Relaxed)
        {
            shared.state.store(STATE_STOP, Ordering::Relaxed);
        }
    }
}

/// Decode `src` end-to-end into the PCM ring. Returns how it ended so
/// the thread can idle, reload, or quit.
fn decode_source(
    src: Source,
    rx: &mpsc::Receiver<Cmd>,
    shared: &Shared,
    prod: &mut HeapProd<f32>,
    out_rate: u32,
) -> DecodeEnd {
    match run_decoder(src, rx, shared, prod, out_rate) {
        Ok(end) => end,
        Err(_) => DecodeEnd::Stopped,
    }
}

fn run_decoder(
    src: Source,
    rx: &mpsc::Receiver<Cmd>,
    shared: &Shared,
    prod: &mut HeapProd<f32>,
    out_rate: u32,
) -> Result<DecodeEnd> {
    // Identity of the source for ReplayGain: a local file carries
    // REPLAYGAIN_* tags; a stream has no file to tag so it only gets
    // on-the-fly analysis (persisted to its stream cache).
    let (rg_file, rg_key) = match &src {
        Source::File(p) => (Some(p.clone()), None),
        Source::Stream { key, .. } => (None, Some(key.clone())),
    };
    let media: Box<dyn MediaSource> = match src {
        Source::File(path) => patch_media(Box::new(std::fs::File::open(path)?)),
        Source::Stream { url, key } => http_source(&url, &key, true)?,
    };
    let mss = MediaSourceStream::new(media, Default::default());
    let mut format = get_probe().probe(
        &Hint::new(),
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
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
        .ok_or("no audio track")?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or("no audio track")?;
    let track_id = track.id;
    let src_rate = params.sample_rate.unwrap();
    let duration = track
        .num_frames
        .map(|n| n as f64 / src_rate as f64)
        .unwrap_or(0.0);
    shared.duration.store(duration.to_bits(), Ordering::Relaxed);
    shared.pos_base.store(0, Ordering::Relaxed);
    shared.pos_consumed.store(0, Ordering::Relaxed);
    shared.eof.store(false, Ordering::Relaxed);
    shared.flush_seq.fetch_add(1, Ordering::Relaxed);
    // Release-ordered AFTER the position reset: an Acquire reader
    // that sees the new generation also sees the zeroed position —
    // never "new source applied, old track's position".
    shared.load_gen.fetch_add(1, Ordering::Release);

    let mut decoder =
        get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default())?;
    let mut resampler = Resampler::new(src_rate, out_rate);
    let mut rg = RgTrack::new();
    let mut denoiser = Denoiser::new();
    let mut filtered = Vec::new();

    loop {
        match rx.try_recv() {
            Ok(Cmd::Stop) | Ok(Cmd::Quit) => return Ok(DecodeEnd::Stopped),
            Ok(Cmd::Load(p)) => return Ok(DecodeEnd::Reload(p)),
            // Seek rides the `Shared::seek` mutex — consumed just
            // below; the command itself needs no action mid-decode.
            Ok(Cmd::Seek) | Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return Ok(DecodeEnd::Quit),
        }
        if let Some(t) = shared.seek.lock().unwrap().take() {
            let _ = format.seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::try_from_secs_f64(t).unwrap_or(Time::ZERO),
                    track_id: Some(track_id),
                },
            );
            decoder.reset();
            resampler.reset();
            denoiser.reset();
            shared.pos_base.store((t * out_rate as f64) as u64, Ordering::Relaxed);
            shared.pos_consumed.store(0, Ordering::Relaxed);
            shared.flush_seq.fetch_add(1, Ordering::Relaxed);
        }
        // Re-evaluate ReplayGain when the options change mid-play
        // (Lacrima's RecomputeReplayGainForCurrentAsync).
        rg.refresh(shared, rg_file.as_deref(), rg_key.as_deref());

        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            // Ok(None) is the clean end-of-stream; a short track may
            // also error before the last checkpoint — either way,
            // finalize the analysis with whatever decoded.
            Ok(None) | Err(_) => {
                rg.finish(shared, rg_key.as_deref());
                return Ok(DecodeEnd::Eof);
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded: GenericAudioBufferRef = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = decoded.spec().clone();
        let frames = decoded.frames() as u64;
        if frames == 0 {
            continue;
        }
        let nch = spec.channels().count().max(1);
        let mut samples = Vec::with_capacity(frames as usize * nch);
        decoded.copy_to_vec_interleaved(&mut samples);
        let stereo: Vec<f32> = if nch == 1 {
            samples.iter().flat_map(|&s| [s, s]).collect()
        } else {
            samples
                .chunks_exact(nch)
                .flat_map(|f| [f[0], f[1]])
                .collect()
        };
        rg.accumulate(&stereo, src_rate, shared, rg_key.as_deref());
        for chunk in resampler.push(&stereo) {
            filtered.clear();
            denoiser.push(
                &chunk,
                shared.denoise.load(Ordering::Relaxed),
                &mut filtered,
            );
            if let Some(cmd) = push_samples(prod, &filtered, rx) {
                return Ok(match cmd {
                    Cmd::Load(p) => DecodeEnd::Reload(p),
                    _ => DecodeEnd::Stopped,
                });
            }
        }
        // Stream preroll gate: once ~1.5s of stereo PCM sits in the
        // ring, unmute the output callback. Eof before that also clears
        // it (decode_thread handles DecodeEnd::Eof).
        if shared.buffering.load(Ordering::Relaxed)
            && prod.occupied_len() >= out_rate as usize * 3
        {
            shared.buffering.store(false, Ordering::Relaxed);
        }
    }
}

/// Feed interleaved stereo samples into the resampler and yield the
/// output chunks. `identity` short-circuits when rates already match.
struct Resampler {
    inner: Option<rubato::Async<f32>>,
    acc: [Vec<f32>; 2],
}

const RESAMPLE_CHUNK: usize = 1024;

impl Resampler {
    fn new(rate_in: u32, rate_out: u32) -> Self {
        let inner = (rate_in != rate_out).then(|| {
            rubato::Async::new_sinc(
                rate_out as f64 / rate_in as f64,
                2.0,
                &rubato::SincInterpolationParameters {
                    sinc_len: 128,
                    f_cutoff: Some(0.95),
                    interpolation: rubato::SincInterpolationType::Linear,
                    oversampling_factor: 128,
                    window: rubato::WindowFunction::BlackmanHarris2,
                },
                RESAMPLE_CHUNK,
                2,
                rubato::FixedAsync::Input,
            )
            .expect("resampler")
        });
        Self {
            inner,
            acc: [Vec::new(), Vec::new()],
        }
    }

    fn reset(&mut self) {
        self.acc = [Vec::new(), Vec::new()];
        if let Some(r) = &mut self.inner {
            let _ = r.reset();
        }
    }

    /// Convert interleaved stereo input into interleaved stereo output
    /// chunks at the output rate.
    fn push(&mut self, stereo: &[f32]) -> Vec<Vec<f32>> {
        if self.inner.is_none() {
            return vec![stereo.to_vec()];
        }
        let r = self.inner.as_mut().unwrap();
        for pair in stereo.chunks_exact(2) {
            self.acc[0].push(pair[0]);
            self.acc[1].push(pair[1]);
        }
        let mut out = Vec::new();
        while self.acc[0].len() >= RESAMPLE_CHUNK {
            let l: Vec<f32> = self.acc[0].drain(..RESAMPLE_CHUNK).collect();
            let rr: Vec<f32> = self.acc[1].drain(..RESAMPLE_CHUNK).collect();
            let pair = [l, rr];
            let input =
                SequentialSliceOfVecs::new(&pair, 2, RESAMPLE_CHUNK).expect("resample input");
            // InterleavedOwned hands the frames back already
            // interleaved — straight into the ring.
            if let Ok(buf) = r.process(&input, None) {
                out.push(buf.take_data());
            }
        }
        out
    }
}

/// Push into the PCM ring, sleeping while full. Returns a command if
/// one arrived while waiting — a full ring (paused playback) must never
/// wedge the decode thread against a pending Load/Stop.
fn push_samples(
    prod: &mut HeapProd<f32>,
    samples: &[f32],
    rx: &mpsc::Receiver<Cmd>,
) -> Option<Cmd> {
    let mut i = 0;
    while i < samples.len() {
        let n = prod.push_slice(&samples[i..]);
        i += n;
        if n == 0 {
            match rx.try_recv() {
                // A seek while the ring is full isn't an interrupt —
                // the decode loop applies the position next pass.
                Ok(Cmd::Seek) => std::thread::sleep(Duration::from_millis(1)),
                Ok(cmd) => return Some(cmd),
                Err(mpsc::TryRecvError::Disconnected) => return Some(Cmd::Quit),
                Err(mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(1))
                }
            }
        }
    }
    None
}

// -------------------------------------------------------------------
// ReplayGain (Lacrima's ApplyReplayGainForFileAsync). Local files read
// REPLAYGAIN_* tags via lofty (it maps Vorbis comments, ID3v2 TXXX and
// iTunes freeform atoms to the same ItemKeys). When no tag exists, the
// decoded PCM doubles as the analyzer: a running RMS level over the
// first ~60s, applied at widening checkpoints so the estimate
// converges early — the Rust-native equivalent of Lacrima's ffmpeg
// `volumedetect` pass, and it works for streams too (theirs skips
// remote sources). Stream results persist in the stream cache meta.
// -------------------------------------------------------------------

/// Decoded-audio seconds at which the running loudness estimate is
/// re-applied; after the last checkpoint the measurement is final.
const RG_CHECKPOINTS: &[f64] = &[5.0, 15.0, 30.0, 60.0];
/// Lacrima's analysis reference (-18 dBFS) and the [-18, +8] clamp it
/// applies to both tag and analyzed adjustments.
const RG_REFERENCE_DB: f64 = -18.0;
const RG_MIN_DB: f64 = -18.0;
const RG_MAX_DB: f64 = 8.0;

struct RgTrack {
    /// The `rg_opts_seq` generation this state was evaluated under —
    /// a settings change bumps it and `refresh` restarts evaluation.
    seq: u64,
    analyzing: bool,
    /// Sum of squared PCM samples / count / decoded seconds so far.
    sum_sq: f64,
    samples: u64,
    decoded_secs: f64,
    next_ckpt: usize,
}

impl RgTrack {
    fn new() -> Self {
        Self {
            // u64::MAX != the initial seq (1), so the first refresh
            // always evaluates the freshly opened source.
            seq: u64::MAX,
            analyzing: false,
            sum_sq: 0.0,
            samples: 0,
            decoded_secs: 0.0,
            next_ckpt: 0,
        }
    }

    /// Re-run the gain decision when the options generation moved:
    /// clear the current adjustment, prefer the tag value (file
    /// sources), else start analyzing — or apply the stream cache's
    /// remembered loudness directly.
    fn refresh(&mut self, shared: &Shared, file: Option<&Path>, stream_key: Option<&str>) {
        let seq = shared.rg_opts_seq.load(Ordering::Relaxed);
        if seq == self.seq {
            return;
        }
        self.seq = seq;
        self.analyzing = false;
        self.sum_sq = 0.0;
        self.samples = 0;
        self.decoded_secs = 0.0;
        self.next_ckpt = 0;
        shared.rg_db.store(0f32.to_bits(), Ordering::Relaxed);
        let opts = *shared.rg_opts.lock().unwrap();
        if !opts.enabled {
            return;
        }
        if opts.use_tags {
            if let Some(db) = file.and_then(|p| rg_tag_gain(p, opts.tag_source)) {
                let adj = (db + opts.preamp_tags_db as f64).clamp(RG_MIN_DB, RG_MAX_DB);
                shared
                    .rg_db
                    .store((adj as f32).to_bits(), Ordering::Relaxed);
                return;
            }
        }
        if !opts.analyze {
            return;
        }
        if let Some(key) = stream_key {
            if let Some(mean) = stream_cache::cache_for(key).rg_mean_db() {
                let adj = (RG_REFERENCE_DB - mean + opts.preamp_analyze_db as f64)
                    .clamp(RG_MIN_DB, RG_MAX_DB);
                shared
                    .rg_db
                    .store((adj as f32).to_bits(), Ordering::Relaxed);
                return;
            }
        }
        self.analyzing = true;
    }

    /// Fold decoded stereo PCM into the loudness estimate and apply it
    /// at each checkpoint. Cheap: gated entirely on `analyzing`, which
    /// clears after the last checkpoint (or never sets without RG).
    fn accumulate(
        &mut self,
        stereo: &[f32],
        src_rate: u32,
        shared: &Shared,
        stream_key: Option<&str>,
    ) {
        if !self.analyzing {
            return;
        }
        for &s in stereo {
            let v = s as f64;
            self.sum_sq += v * v;
        }
        self.samples += stereo.len() as u64;
        self.decoded_secs += stereo.len() as f64 / 2.0 / src_rate as f64;
        if self.next_ckpt < RG_CHECKPOINTS.len()
            && self.decoded_secs >= RG_CHECKPOINTS[self.next_ckpt]
        {
            self.next_ckpt += 1;
            let mean = self.mean_db();
            self.apply(mean, shared);
            if self.next_ckpt >= RG_CHECKPOINTS.len() {
                self.persist(mean, stream_key);
            }
        }
    }

    /// Final application at EOF (or the last checkpoint): short
    /// tracks still get their gain from whatever decoded.
    fn finish(&mut self, shared: &Shared, stream_key: Option<&str>) {
        if !self.analyzing || self.samples == 0 {
            return;
        }
        let mean = self.mean_db();
        self.apply(mean, shared);
        self.persist(mean, stream_key);
    }

    fn mean_db(&self) -> f64 {
        10.0 * (self.sum_sq / self.samples.max(1) as f64)
            .max(1e-12)
            .log10()
    }

    fn apply(&mut self, mean_db: f64, shared: &Shared) {
        let opts = *shared.rg_opts.lock().unwrap();
        let adj =
            (RG_REFERENCE_DB - mean_db + opts.preamp_analyze_db as f64).clamp(RG_MIN_DB, RG_MAX_DB);
        shared
            .rg_db
            .store((adj as f32).to_bits(), Ordering::Relaxed);
    }

    /// A stream's measured loudness survives in its cache meta — the
    /// next playthrough applies it at the first packet instead of
    /// waiting ~5s for the first checkpoint.
    fn persist(&mut self, mean_db: f64, stream_key: Option<&str>) {
        self.analyzing = false;
        if let Some(key) = stream_key {
            stream_cache::cache_for(key).set_rg_mean_db(mean_db);
        }
    }
}

/// `REPLAYGAIN_ALBUM_GAIN` / `REPLAYGAIN_TRACK_GAIN` from any tag on
/// the file. Lacrima's source pick is asymmetric: "Album" tries album
/// gain first and falls back to track gain, while "Track" reads the
/// track gain only — no album fallback.
fn rg_tag_gain(path: &Path, tag_source: u8) -> Option<f64> {
    let tagged = lofty::read_from_path(path).ok()?;
    for tag in tagged.tags() {
        let album = tag
            .get_string(ItemKey::ReplayGainAlbumGain)
            .and_then(parse_gain_db);
        let track = tag
            .get_string(ItemKey::ReplayGainTrackGain)
            .and_then(parse_gain_db);
        let pick = if tag_source == 1 {
            album.or(track)
        } else {
            track
        };
        if pick.is_some() {
            return pick;
        }
    }
    None
}

/// Tag values look like "-3.00 dB" / "+2.5" — Lacrima's StripDb.
fn parse_gain_db(s: &str) -> Option<f64> {
    let t = s.trim();
    let t = t
        .strip_suffix("dB")
        .or_else(|| t.strip_suffix("db"))
        .unwrap_or(t)
        .trim();
    t.parse::<f64>().ok()
}

// -------------------------------------------------------------------
// HTTP-backed MediaSource on top of `StreamCache` — a downloader Feed
// (a few range lanes) writes every fetched byte into the persistent
// cache; readers hit disk for fetched ranges and condvar-wait at the
// live edge. symphonia's demuxers issue constant small backward/forward
// seeks — those are file reads now, not reconnects (the "slow game"
// crackle). A seek to a truly missing range restarts the feed there.
// -------------------------------------------------------------------

/// Downloader lanes and segment size — each lane owns a disjoint 1MB
/// segment so the cache fills ~4x a single connection.
const FEED_LANES: usize = 4;
const SEGMENT: u64 = 1024 * 1024;
const FEED_CHUNK: usize = 64 * 1024;
const HTTP_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Shared agent for the streaming path — bare `ureq::get` sets no
/// deadline at all, so a stalled googlevideo socket blocked the
/// decode thread inside `http_source` forever (the queued `Cmd::Load`
/// never returns, wedging every later open). Connect/response/body
/// are all bounded; mid-body stalls still surface through the lane
/// reader as errors the stalled() logic already handles.
fn http_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(15)))
            .timeout_recv_body(Some(Duration::from_secs(60)))
            .build()
            .into()
    })
}

struct HttpSource {
    url: String,
    /// Absolute offset the consumer reads next.
    pos: u64,
    /// The shared persistent cache (per item url, across sessions).
    cache: Arc<stream_cache::StreamCache>,
    /// Active downloader — feeders own it, read-only consumers (the
    /// waveform worker) never start one and stop at the first gap.
    feed: Option<Arc<Feed>>,
    feeder: bool,
    /// Consecutive failed reconnects — reset by any served byte so a
    /// dead stream gives up instead of reconnect-looping forever.
    fails: u32,
    /// Whether this consumer has ever seen a live downloader —
    /// non-feeders only treat `live == None` as finished AFTER one
    /// existed (the playback feed starts a beat after the waveform
    /// worker spawns; an empty `live` up front is "not yet", not
    /// "never").
    saw_live: bool,
    /// Non-feeder grace deadline for waiting on `saw_live` — beyond it
    /// the consumer gives up and reports EOF.
    live_grace: Option<std::time::Instant>,
    /// Progress watchdog for waits on uncached ranges: total cached
    /// bytes last time we checked and when it last grew. A live feed
    /// that hasn't appended anything anywhere for a while is stalled
    /// — a read-only consumer reports EOF rather than hang forever.
    last_covered: u64,
    progress_at: std::time::Instant,
}

/// A boxed HTTP source for off-band decoding — the waveform worker
/// consumes the same cache as playback: no second download (the stall
/// source) and instant fill for already-cached bytes.
pub(crate) fn http_source(
    url: &str,
    key: &str,
    feeder: bool,
) -> std::io::Result<Box<dyn MediaSource>> {
    let src = HttpSource::open(url, key, feeder)?;
    let patches = src.atom_patches();
    if patches.is_empty() {
        Ok(Box::new(src))
    } else {
        Ok(Box::new(PatchSource {
            inner: Box::new(src),
            pos: 0,
            patches,
        }))
    }
}

// -------------------------------------------------------------------
// Malformed-MP4 repair — some googlevideo muxes inject a `uuid` box
// without shrinking `mdat`'s declared size, so the last root atom
// overshoots the real stream length. symphonia's seekable mp4 probe
// walks EVERY root atom (`AtomIterator::next` ignore_bytes-es to the
// declared end before checking the length) and dies on EOF. Cached
// bytes are left untouched; the size field is spliced into served
// reads instead so every consumer sees a consistent container.
// -------------------------------------------------------------------

/// How much of the head is walked for atom headers — covers
/// `ftyp`+`uuid`+`moov`+`mdat` for typical muxes.
const PATCH_HEAD: u64 = 192 * 1024;

/// A rewritten atom size field: `(field offset, corrected size, field
/// width)` — width 4 for normal sizes, 8 for `size==1` extended sizes.
struct AtomPatch {
    off: u64,
    value: u64,
    width: u8,
}

/// Walk top-level ISO-BMFF atom headers; a box whose declared size
/// runs past `len` gets its size field clamped to the stream end
/// (it necessarily is the last box — nothing valid can follow it).
/// Only scans when the data starts with an `ftyp` brand and every
/// walked type looks like a fourcc — other containers are left alone.
fn atom_patch_scan(head: &[u8], len: u64) -> Vec<AtomPatch> {
    let mut out = Vec::new();
    if head.len() < 8 || &head[4..8] != b"ftyp" {
        return out;
    }
    let mut pos = 0u64;
    while (pos as usize) + 8 <= head.len() && pos + 8 <= len {
        let s = &head[pos as usize..];
        let sz32 = u32::from_be_bytes(s[..4].try_into().unwrap());
        if !s[4..8].iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            break;
        }
        let (size, hdr, foff, width) = if sz32 == 1 {
            if (pos as usize) + 16 > head.len() || pos + 16 > len {
                break;
            }
            (
                u64::from_be_bytes(s[8..16].try_into().unwrap()),
                16u64,
                pos + 8,
                8u8,
            )
        } else if sz32 == 0 {
            // Runs to EOF by definition — can never overshoot.
            (len - pos, 8u64, pos, 4u8)
        } else {
            (sz32 as u64, 8u64, pos, 4u8)
        };
        if size < hdr {
            break;
        }
        let end = pos.saturating_add(size);
        if end > len {
            out.push(AtomPatch {
                off: foff,
                value: len - pos,
                width,
            });
            break;
        }
        pos = end;
    }
    out
}

/// Splice corrected size fields over the served range `[pos, pos+len)`.
fn apply_patches(pos: u64, buf: &mut [u8], patches: &[AtomPatch]) {
    let end = pos + buf.len() as u64;
    for p in patches {
        let lo = pos.max(p.off);
        let hi = end.min(p.off + p.width as u64);
        if lo >= hi {
            continue;
        }
        let field = p.value.to_be_bytes();
        let field = &field[8 - p.width as usize..];
        buf[(lo - pos) as usize..(hi - pos) as usize]
            .copy_from_slice(&field[(lo - p.off) as usize..(hi - p.off) as usize]);
    }
}

/// MediaSource wrapper applying `AtomPatch`es to served reads — the
/// inner source's bytes pass through untouched except for the fixed
/// size fields.
struct PatchSource {
    inner: Box<dyn MediaSource>,
    pos: u64,
    patches: Vec<AtomPatch>,
}

impl std::io::Read for PatchSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        apply_patches(self.pos, &mut buf[..n], &self.patches);
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for PatchSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

impl MediaSource for PatchSource {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// Repair pass for any seekable `MediaSource` — reads the head through
/// the source itself, rewinds, and wraps it in `PatchSource` when an
/// overshooting root atom was found. Local files get the same repair
/// as streams (a downloaded malformed mp4 is just as unproble).
pub(crate) fn patch_media(mut media: Box<dyn MediaSource>) -> Box<dyn MediaSource> {
    let Some(len) = media.byte_len() else {
        return media;
    };
    let want = PATCH_HEAD.min(len) as usize;
    let mut head = vec![0u8; want];
    let mut have = 0usize;
    while have < want {
        match media.read(&mut head[have..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => have += n,
        }
    }
    if media.seek(std::io::SeekFrom::Start(0)).is_err() {
        return media;
    }
    let patches = atom_patch_scan(&head[..have], len);
    if patches.is_empty() {
        media
    } else {
        Box::new(PatchSource {
            inner: media,
            pos: 0,
            patches,
        })
    }
}

/// A downloader: `FEED_LANES` workers claim disjoint `SEGMENT` ranges
/// and append bytes into the cache. After reaching the end each lane
/// runs a sweep over its own stride of the whole file, healing gaps a
/// probe-seek restart left below the feed base — the cache keeps
/// building until complete.
struct Feed {
    id: u64,
    url: String,
    cache: Arc<stream_cache::StreamCache>,
    /// Next segment start to claim.
    next: AtomicU64,
    /// Segment starts a lane is currently fetching — the sweep pass
    /// must not double-fetch a range another lane still owns.
    inflight: Mutex<std::collections::HashSet<u64>>,
    cancel: AtomicBool,
}

impl Feed {
    fn start(url: &str, pos: u64, cache: &Arc<stream_cache::StreamCache>) -> Arc<Self> {
        let feed = Arc::new(Feed {
            id: stream_cache::next_live_id(),
            url: url.to_string(),
            cache: cache.clone(),
            next: AtomicU64::new(pos),
            inflight: Mutex::new(std::collections::HashSet::new()),
            cancel: AtomicBool::new(false),
        });
        cache.live_begin(feed.id, pos, FEED_LANES);
        for lane in 0..FEED_LANES {
            let f = feed.clone();
            std::thread::Builder::new()
                .name("aes-http-feed".into())
                .spawn(move || f.run_lane(lane))
                .ok();
        }
        feed
    }

    /// Download whatever of `[off, off+SEGMENT)` (clamped to the
    /// stream length) is still missing: fully cached segments are
    /// skipped, partially cached ones download only the tail. Returns
    /// false on a transport error (cancel/short-body are not errors).
    fn fetch_segment(&self, off: u64) -> bool {
        let end = self
            .cache
            .len()
            .map(|l| (off + SEGMENT).min(l))
            .unwrap_or(off + SEGMENT);
        if off >= end {
            return true;
        }
        // Resume at the coverage edge: fully cached segments are
        // skipped, partially cached ones download only the tail —
        // resumed feeds fill gaps, never re-fetch known bytes.
        let mut at = {
            let inner = self.cache.inner();
            match inner.covered_end(off) {
                Some(e) if e >= end => return true,
                Some(e) => e,
                None => off,
            }
        };
        // One lane owns a segment at a time — the sweep pass can meet
        // a segment still streaming under `next`.
        if !self.inflight.lock().unwrap().insert(off) {
            return true;
        }
        let mut ok = true;
        let req = http_agent()
            .get(&self.url)
            .header("Range", &format!("bytes={}-{}", at, end - 1))
            .header("User-Agent", HTTP_UA);
        match req.call() {
            Ok(resp) => {
                let mut rd = resp.into_body().into_reader();
                let mut chunk = vec![0u8; FEED_CHUNK];
                loop {
                    if self.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    match std::io::Read::read(&mut rd, &mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            self.cache.append(at, &chunk[..n]);
                            at += n as u64;
                        }
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            Err(_) => ok = false,
        }
        self.inflight.lock().unwrap().remove(&off);
        ok
    }

    fn run_lane(&self, lane: usize) {
        let mut err = false;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }
            let off = self.next.fetch_add(SEGMENT, Ordering::Relaxed);
            if matches!(self.cache.len(), Some(l) if off >= l) {
                break;
            }
            if !self.fetch_segment(off) {
                err = true;
                break;
            }
        }
        // Sweep pass: gaps a probe-seek restart left below the feed
        // base (and sections the user skipped) are healed so the file
        // eventually caches complete. Each lane owns a disjoint
        // stride — no duplicate fetches, failures just leave the gap.
        if !err {
            if let Some(len) = self.cache.len() {
                let stride = SEGMENT * FEED_LANES as u64;
                let mut seg = lane as u64 * SEGMENT;
                while seg < len && !self.cancel.load(Ordering::Relaxed) {
                    let _ = self.fetch_segment(seg);
                    seg += stride;
                }
            }
        }
        self.cache.live_worker_done(self.id, err);
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl HttpSource {
    /// `feeder` = the playback consumer: it owns the download lanes and
    /// may restart them on misses. Non-feeders (waveform) are read-only:
    /// they consume what's cached and stop at gaps.
    fn open(url: &str, key: &str, feeder: bool) -> std::io::Result<Self> {
        let cache = stream_cache::cache_for(key);
        let mut src = Self {
            url: url.to_string(),
            pos: 0,
            cache,
            feed: None,
            feeder,
            fails: 0,
            saw_live: false,
            live_grace: (!feeder)
                .then(|| std::time::Instant::now() + std::time::Duration::from_secs(15)),
            last_covered: 0,
            progress_at: std::time::Instant::now(),
        };
        if feeder {
            // Headers give us the total length + format signature for
            // cache validation; the feed then starts at the read pos.
            let len = Self::head_len(url)?;
            src.cache.ensure_format(len, &stream_cache::format_sig(url));
            src.start_feed(0);
        } else {
            // The feeder's headers set `len` a beat after our spawn —
            // wait briefly so `is_seekable`/`byte_len` are truthful
            // for the demuxer (mp4/webm probing needs seeks).
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(3);
            while src.cache.len().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        Ok(src)
    }

    /// Scan the stream's head for a malformed root atom overshooting
    /// the real length (see `atom_patch_scan`). Head bytes come from
    /// the shared cache when already fetched, topped up by a single
    /// ranged GET — the feed downloads the same region in parallel
    /// anyway, so this costs at most one extra connection.
    fn atom_patches(&self) -> Vec<AtomPatch> {
        let Some(len) = self.cache.len() else {
            return Vec::new();
        };
        let want = PATCH_HEAD.min(len);
        let mut head = vec![0u8; want as usize];
        let mut have = {
            let mut inner = self.cache.inner();
            inner.covered_read(0, &mut head).unwrap_or(0)
        };
        if (have as u64) < want {
            let resp = http_agent()
                .get(&self.url)
                .header("Range", &format!("bytes={}-{}", have, want - 1))
                .header("User-Agent", HTTP_UA)
                .call();
            if let Ok(resp) = resp {
                let mut rd = resp.into_body().into_reader();
                while have < head.len() {
                    match std::io::Read::read(&mut rd, &mut head[have..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => have += n,
                    }
                }
            }
        }
        atom_patch_scan(&head[..have], len)
    }

    /// Total length via a 1-byte GET (googlevideo honours Range and
    /// answers Content-Range without paying for a second connection —
    /// lane 0 opens the real download right after).
    fn head_len(url: &str) -> std::io::Result<Option<u64>> {
        let resp = http_agent()
            .get(url)
            .header("Range", "bytes=0-0")
            .header("User-Agent", HTTP_UA)
            .call()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let len = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next()?.parse().ok())
            .or_else(|| {
                resp.headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
            });
        Ok(len)
    }

    /// (Re)start the download lanes at `pos`. Dropping the old feed
    /// cancels its lanes; its late appends are idempotent (ranges are
    /// offset-accurate).
    fn start_feed(&mut self, pos: u64) {
        self.feed = Some(Feed::start(&self.url, pos, &self.cache));
    }

    /// Read-only consumers wait on gaps a forward-running feed will
    /// fill later — but only while the feed is actually making
    /// progress somewhere. ~15s without any new cached byte means a
    /// stalled downloader; report EOF instead of hanging the waveform
    /// worker (and its spinner) forever. `covered` is the current
    /// total from the cache lock the caller already holds.
    fn stalled(&mut self, covered: u64) -> bool {
        if covered != self.last_covered {
            self.last_covered = covered;
            self.progress_at = std::time::Instant::now();
            false
        } else {
            self.progress_at.elapsed() > std::time::Duration::from_secs(15)
        }
    }
}

impl Drop for HttpSource {
    fn drop(&mut self) {
        if let Some(f) = &self.feed {
            self.cache.clear_live(f.id);
        }
    }
}

impl std::io::Read for HttpSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        const MAX_FAILS: u32 = 4;
        loop {
            let mut inner = self.cache.inner();
            if let Some(n) = inner.covered_read(self.pos, buf) {
                self.pos += n as u64;
                self.fails = 0;
                return Ok(n);
            }
            // Reads at or past the known stream end are EOF — no feed
            // lane will ever produce them, so don't wait for one.
            if matches!(inner.total_len(), Some(l) if self.pos >= l) {
                return Ok(0);
            }
            if inner.live().is_some() {
                self.saw_live = true;
            }
            match inner.live() {
                // The live feed is at or ahead of us — wait for data.
                // (pos == base is the steady case; pos inside a gap a
                // lane is still filling also lands here.)
                Some((base, false, _)) if self.pos >= base => {
                    let total = inner.ranges_total();
                    drop(inner);
                    if self.stalled(total) {
                        // A "live" feed that produced nothing for ~15s
                        // is a hung connection — ureq sets no read
                        // deadline by default, so its lanes block
                        // forever and the live entry never turns done.
                        // A read-only consumer bails to EOF; the
                        // playback feeder restarts the lanes (bounded
                        // by MAX_FAILS), then EOFs too — a dead stream
                        // must still end the track so the queue can
                        // advance instead of buffering forever.
                        if !self.feeder || self.fails >= MAX_FAILS {
                            return Ok(0);
                        }
                        self.fails += 1;
                        // The restarted feed needs its own stall
                        // window — the covered total is unchanged, so
                        // without resetting the clock the next wait
                        // instantly re-detects the same stall and
                        // burns every retry inside one read call.
                        self.progress_at = std::time::Instant::now();
                        self.start_feed(self.pos);
                    }
                    let cache = self.cache.clone();
                    let g = cache.inner();
                    let _ = cache
                        .ready()
                        .wait_timeout(g, Duration::from_millis(500));
                }
                // Live feed exists but feeds strictly ahead — the gap
                // below it is fixed by the feeder restarting there; a
                // read-only consumer waits for the feed's sweep pass
                // (or the playback reader restarting the feed here) to
                // cover it instead of dying on an instant EOF.
                Some((base, _, _)) if self.pos < base => {
                    let total = inner.ranges_total();
                    drop(inner);
                    if !self.feeder {
                        if self.stalled(total) {
                            return Ok(0);
                        }
                        let cache = self.cache.clone();
                        let g = cache.inner();
                        let _ = cache
                            .ready()
                            .wait_timeout(g, Duration::from_millis(500));
                        continue;
                    }
                    if self.fails >= MAX_FAILS {
                        return Ok(0);
                    }
                    self.fails += 1;
                    self.start_feed(self.pos);
                }
                // Live feed dead-ended: socket error or early EOF.
                Some((_, done, err)) => {
                    let promised = matches!(inner.total_len(), Some(l) if self.pos < l);
                    drop(inner);
                    if !self.feeder {
                        return Ok(0);
                    }
                    if (err || (done && promised)) && self.fails < MAX_FAILS {
                        self.fails += 1;
                        self.start_feed(self.pos);
                        continue;
                    }
                    if err {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "stream download failed",
                        ));
                    }
                    return Ok(0);
                }
                // No live feed: a non-feeder that has never SEEN one
                // yet is probably just ahead of the playback feed's
                // startup — wait within the grace window instead of
                // reporting an instant EOF (that made the waveform's
                // probe fail). Feeders restart at their position.
                None => {
                    if !self.feeder
                        && !self.saw_live
                        && self.live_grace.is_some_and(|d| std::time::Instant::now() < d)
                    {
                        let (g, _) = self
                            .cache
                            .ready()
                            .wait_timeout(inner, std::time::Duration::from_millis(300))
                            .unwrap();
                        inner = g;
                        continue;
                    }
                    drop(inner);
                    if !self.feeder || self.fails >= MAX_FAILS {
                        return Ok(0);
                    }
                    self.fails += 1;
                    self.start_feed(self.pos);
                }
            }
        }
    }
}

impl std::io::Seek for HttpSource {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            std::io::SeekFrom::Start(n) => n,
            std::io::SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            std::io::SeekFrom::End(d) => match self.cache.len() {
                Some(len) => (len as i64 + d).max(0) as u64,
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "stream length unknown",
                    ))
                }
            },
        };
        if target == self.pos {
            return Ok(target);
        }
        // Any fetched range is a file seek — the running feed keeps
        // filling ahead of wherever it is (sparse gaps stay gaps until
        // a consumer asks for them).
        let covered = {
            let mut inner = self.cache.inner();
            inner.covered_end(target).is_some()
        };
        self.pos = target;
        if self.feeder && !covered {
            self.start_feed(target);
        }
        Ok(target)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        self.cache.len().is_some()
    }
    fn byte_len(&self) -> Option<u64> {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repro for a failing stream: AES_TEST_URL=<resolved media url>
    /// AES_TEST_KEY=<item key> — opens the cached HTTP source exactly
    /// like playback and runs symphonia's probe + first decodes.
    #[test]
    #[ignore]
    fn stream_probe_repro() {
        let media: Box<dyn MediaSource> = if let Ok(file) = std::env::var("AES_TEST_FILE") {
            patch_media(Box::new(std::fs::File::open(&file).expect("open file")))
        } else {
            let url = std::env::var("AES_TEST_URL").expect("AES_TEST_URL");
            let key = std::env::var("AES_TEST_KEY").unwrap_or_else(|_| url.clone());
            http_source(&url, &key, true).expect("http_source")
        };
        let mss = MediaSourceStream::new(media, Default::default());
        let mut format = get_probe()
            .probe(
                &Hint::new(),
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .expect("probe");
        let track = format
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
            .expect("audio track")
            .clone();
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).expect("audio");
        eprintln!("track: {:?} rate={:?}", params.codec, params.sample_rate);
        let mut dec = get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .expect("decoder");
        for _ in 0..20 {
            match format.next_packet() {
                Ok(Some(p)) if p.track_id == track.id => {
                    dec.decode(&p).expect("decode");
                }
                Ok(_) => {}
                Err(e) => panic!("packet read failed: {e}"),
            }
        }
    }

    /// A "live" feed that produces nothing is a hung socket (ureq
    /// sets no read deadline by default, so its lanes can block
    /// forever and the live entry never turns done). The playback
    /// feeder used to wait on it unconditionally — the decoder never
    /// saw EOF, the track never flagged Finished and the queue
    /// stalled mid-album. Now a stall restarts the lanes a bounded
    /// number of times, then reports EOF/error so the track still
    /// ends and auto-advance can fire.
    #[test]
    fn feeder_stall_ends_instead_of_hanging() {
        let cache = stream_cache::cache_for("aes-test-feeder-stall");
        // Registered live, never producing — exactly the hung state.
        cache.live_begin(stream_cache::next_live_id(), 0, FEED_LANES);
        let mut src = HttpSource {
            // Instantly refused — restarted lanes die fast instead of
            // downloading real data mid-test.
            url: "http://127.0.0.1:1/unreachable".to_string(),
            pos: 0,
            cache: cache.clone(),
            feed: None,
            feeder: true,
            fails: 0,
            saw_live: true,
            live_grace: None,
            last_covered: 0,
            progress_at: std::time::Instant::now() - std::time::Duration::from_secs(60),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = tx.send(std::io::Read::read(&mut src, &mut buf));
        });
        // Without the stall restart this never returns — a few
        // bounded retries against a refused port finish in seconds.
        // Ok(0) (EOF) and Err both end the track; only a hang loses.
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("feeder read hung on a stalled live feed");
        assert!(matches!(res, Ok(0) | Err(_)), "unexpected: {res:?}");
    }
}
